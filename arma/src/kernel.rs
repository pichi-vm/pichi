//! Kernel input handling: arch inference + format validation.
//!
//! Arma accepts only direct-boot kernel formats that tatu can hand
//! off to:
//!
//! - **x86_64**: a Linux bzImage (HdrS magic at offset 0x202, boot
//!   protocol >= 2.12, LOADED_HIGH set). Passed through whole; tatu
//!   reads `setup_sects` at runtime to compute the 64-bit entry.
//! - **aarch64**: a raw arm64 `Image` (ARM\x64 magic at offset 56).
//!   Passed through whole; tatu jumps to offset 0. An arm64 EFI-zboot
//!   wrapper (`CONFIG_EFI_ZBOOT`: `MZ` + `zimg`, a gzip-compressed Image —
//!   the form distro `vmlinuz` ships, e.g. Alpine `vmlinuz-virt`) is
//!   unwrapped to its raw Image first; see [`unwrap_zboot`].
//!
//! A PE-wrapped Linux `vmlinuz.efi` that is *not* zboot is rejected with a
//! hint to extract the raw Image.

use std::io::Read;

use flate2::read::GzDecoder;
use thiserror::Error;

/// Target guest architecture, inferred from the kernel file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// PE `FileHeader.Machine` value for this arch.
    pub(crate) const fn pe_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 0x8664,
            Arch::Aarch64 => 0xAA64,
        }
    }
}

/// Errors from kernel parsing.
#[derive(Debug, Error)]
pub(crate) enum KernelError {
    #[error("kernel file too small ({0} bytes) to determine format")]
    TooSmall(usize),

    #[error(
        "kernel format not recognized. Expected bzImage (HdrS at 0x202) or arm64 Image (ARM\\x64 at offset 56). \
         If this is vmlinuz.efi, extract the raw Image first (arma does not run EFI services)."
    )]
    Unrecognized,

    #[error("bzImage boot protocol {0:#06x} too old (need >= 2.12 for 64-bit boot)")]
    OldProtocol(u16),

    #[error("bzImage missing LOADED_HIGH flag (not a 64-bit-capable bzImage)")]
    NotLoadedHigh,

    #[error("bzImage kernel_alignment ({0:#x}) exceeds arma's supported x86 bzImage alignment")]
    KernelAlignmentTooLarge(u32),

    #[error("bzImage kernel_alignment ({0:#x}) is not a non-zero power of two")]
    KernelAlignmentInvalid(u32),

    #[error("EFI-zboot payload out of bounds (offset {offset:#x}, size {size:#x}, file {file} bytes)")]
    ZbootMalformed {
        offset: usize,
        size: usize,
        file: usize,
    },

    #[error("EFI-zboot uses unsupported compression {0:?}; only gzip is supported")]
    ZbootCompression(String),

    #[error("EFI-zboot payload gunzip failed: {0}")]
    ZbootDecompress(String),
}

const BZIMAGE_HDRS_MAGIC: u32 = 0x5372_6448; // "HdrS" LE at 0x202
const BZIMAGE_HDRS_OFFSET: usize = 0x202;
const BZIMAGE_VERSION_OFFSET: usize = 0x206;
const BZIMAGE_LOADFLAGS_OFFSET: usize = 0x211;
const BZIMAGE_MIN_PROTOCOL: u16 = 0x020C; // 2.12

// Setup header fields used by arma to size the kernel's RAM footprint.
// See Documentation/arch/x86/boot.rst for the wire layout.
const BZIMAGE_SETUP_SECTS_OFFSET: usize = 0x1F1; // u8
const BZIMAGE_KERNEL_ALIGNMENT_OFFSET: usize = 0x230; // u32
const BZIMAGE_PREF_ADDRESS_OFFSET: usize = 0x258; // u64
const BZIMAGE_INIT_SIZE_OFFSET: usize = 0x260; // u32

/// Maximum kernel_alignment arma can satisfy for x86 bzImage inputs.
/// PMI LARGE sections remain 2 MiB-granular, but distro bzImages may
/// request a larger decompressor alignment. arma satisfies that by
/// selecting a `.linux` GPA with the requested alignment.
const MAX_KERNEL_ALIGNMENT: u32 = 16 * 1024 * 1024;

const ARM64_IMAGE_MAGIC: u32 = 0x644D_5241; // "ARM\x64" LE at offset 56
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;
const ARM64_IMAGE_SIZE_OFFSET: usize = 16; // u64 LE: effective image size (text + BSS)

// arm64 EFI-zboot header (`CONFIG_EFI_ZBOOT`). Offsets verified against a
// real Alpine `vmlinuz-virt`: "MZ" at 0, "zimg" at 4, u32 payload offset at
// 8, u32 payload size at 12, NUL-padded compression name at 24.
const ZBOOT_ZIMG_OFFSET: usize = 4;
const ZBOOT_PAYLOAD_OFFSET_FIELD: usize = 8;
const ZBOOT_PAYLOAD_SIZE_FIELD: usize = 12;
const ZBOOT_COMP_OFFSET: usize = 24;
const ZBOOT_COMP_LEN: usize = 32;
const ZBOOT_HEADER_MIN: usize = ZBOOT_COMP_OFFSET + ZBOOT_COMP_LEN; // 56

/// Result of parsing a kernel file.
#[derive(Debug, Clone)]
pub(crate) struct Parsed {
    pub(crate) arch: Arch,
    /// x86 only: setup-header fields needed to size the `.linux`
    /// section's RAM footprint (decompressor scratch buffer).
    pub(crate) bzimage: Option<BzImageMeta>,
    /// aarch64 only: the Image header's `image_size` (text + BSS) — the RAM
    /// the kernel needs at runtime, which exceeds the file when the BSS isn't
    /// in the file. `0` if the header leaves it unspecified. The `.linux`
    /// footprint must be `max(file_size, image_size)` or the BSS is unbacked.
    pub(crate) aarch64_image_size: Option<u64>,
}

/// bzImage setup-header fields relevant to RAM sizing and placement.
/// For a relocatable kernel the decompressor runs it at
/// `rbp = max(ceil(load_addr + setup_bytes, kernel_alignment),
/// pref_address)` and then uses `[rbp, rbp + init_size)` as a scratch
/// buffer that includes its own runtime stack (see
/// `arch/x86/boot/compressed/head_64.S`). `pref_address` is the kernel's
/// preferred (link) address: a kernel loaded **below** it is relocated
/// **up** to it. arma therefore floors `.linux`'s load GPA to
/// `pref_address` (see `layout::compute_x86`) so `rbp == load_addr +
/// slack` and the existing `(slack + init_size)` footprint backs the
/// whole runtime region — otherwise the relocated kernel runs off the
/// top of the backed island.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BzImageMeta {
    pub(crate) init_size: u32,
    pub(crate) kernel_alignment: u32,
    pub(crate) setup_sects: u8,
    /// Preferred (link) physical address. The kernel is relocated up to
    /// this if loaded lower; arma uses it as the `.linux` GPA floor.
    pub(crate) pref_address: u64,
}

impl BzImageMeta {
    /// Setup-area size in bytes — the real-mode portion arma loads at
    /// `[load_addr, load_addr + setup_bytes)` before the protected-mode
    /// kernel begins.
    pub(crate) fn setup_bytes(self) -> u64 {
        (u64::from(self.setup_sects) + 1) * 512
    }
}

/// Detect and validate the guest architecture from a kernel image's
/// header bytes. Arma never holds onto the kernel bytes itself; the
/// caller passes the original buffer through to layout and PE
/// emission.
pub(crate) fn parse(bytes: &[u8]) -> Result<Parsed, KernelError> {
    if bytes.len() < 64 {
        return Err(KernelError::TooSmall(bytes.len()));
    }

    // Check arm64 Image first (cheap, single u32 at fixed offset).
    let arm64_magic = u32::from_le_bytes(
        bytes[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    );
    if arm64_magic == ARM64_IMAGE_MAGIC {
        let image_size = u64::from_le_bytes(
            bytes[ARM64_IMAGE_SIZE_OFFSET..ARM64_IMAGE_SIZE_OFFSET + 8]
                .try_into()
                .expect("slice is 8 bytes"),
        );
        return Ok(Parsed {
            arch: Arch::Aarch64,
            bzimage: None,
            aarch64_image_size: Some(image_size),
        });
    }

    // Check bzImage. Needs the full setup header (through init_size at 0x260+4 = 0x264).
    if bytes.len() >= 0x264 {
        let hdrs_magic = u32::from_le_bytes(
            bytes[BZIMAGE_HDRS_OFFSET..BZIMAGE_HDRS_OFFSET + 4]
                .try_into()
                .expect("slice is 4 bytes"),
        );
        if hdrs_magic == BZIMAGE_HDRS_MAGIC {
            let version = u16::from_le_bytes(
                bytes[BZIMAGE_VERSION_OFFSET..BZIMAGE_VERSION_OFFSET + 2]
                    .try_into()
                    .expect("slice is 2 bytes"),
            );
            if version < BZIMAGE_MIN_PROTOCOL {
                return Err(KernelError::OldProtocol(version));
            }
            let loadflags = bytes[BZIMAGE_LOADFLAGS_OFFSET];
            if loadflags & 0x01 == 0 {
                return Err(KernelError::NotLoadedHigh);
            }
            let setup_sects = bytes[BZIMAGE_SETUP_SECTS_OFFSET];
            let kernel_alignment = u32::from_le_bytes(
                bytes[BZIMAGE_KERNEL_ALIGNMENT_OFFSET..BZIMAGE_KERNEL_ALIGNMENT_OFFSET + 4]
                    .try_into()
                    .expect("slice is 4 bytes"),
            );
            let init_size = u32::from_le_bytes(
                bytes[BZIMAGE_INIT_SIZE_OFFSET..BZIMAGE_INIT_SIZE_OFFSET + 4]
                    .try_into()
                    .expect("slice is 4 bytes"),
            );
            let pref_address = u64::from_le_bytes(
                bytes[BZIMAGE_PREF_ADDRESS_OFFSET..BZIMAGE_PREF_ADDRESS_OFFSET + 8]
                    .try_into()
                    .expect("slice is 8 bytes"),
            );
            if !kernel_alignment.is_power_of_two() {
                return Err(KernelError::KernelAlignmentInvalid(kernel_alignment));
            }
            if kernel_alignment > MAX_KERNEL_ALIGNMENT {
                return Err(KernelError::KernelAlignmentTooLarge(kernel_alignment));
            }
            return Ok(Parsed {
                arch: Arch::X86_64,
                bzimage: Some(BzImageMeta {
                    init_size,
                    kernel_alignment,
                    setup_sects,
                    pref_address,
                }),
                aarch64_image_size: None,
            });
        }
    }

    Err(KernelError::Unrecognized)
}

/// Unwrap an arm64 EFI-zboot kernel to its raw `Image`.
///
/// `CONFIG_EFI_ZBOOT` kernels are a tiny EFI stub (`MZ`) tagged `zimg` at
/// offset 4, wrapping a compressed raw `Image`. Distro arm64 `vmlinuz`
/// ships this form (e.g. Alpine `vmlinuz-virt`). Input that is not zboot
/// passes through unchanged, so this is safe to call on any kernel before
/// [`parse`]. Only gzip is handled (what the arm64 zboot kernels we target
/// use); other compression schemes are an error rather than a silent pass.
pub(crate) fn unwrap_zboot(input: Vec<u8>) -> Result<Vec<u8>, KernelError> {
    let is_zboot = input.len() >= ZBOOT_HEADER_MIN
        && &input[0..2] == b"MZ"
        && &input[ZBOOT_ZIMG_OFFSET..ZBOOT_ZIMG_OFFSET + 4] == b"zimg";
    if !is_zboot {
        return Ok(input);
    }

    let offset = u32::from_le_bytes(
        input[ZBOOT_PAYLOAD_OFFSET_FIELD..ZBOOT_PAYLOAD_OFFSET_FIELD + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    ) as usize;
    let size = u32::from_le_bytes(
        input[ZBOOT_PAYLOAD_SIZE_FIELD..ZBOOT_PAYLOAD_SIZE_FIELD + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    ) as usize;
    let end = offset
        .checked_add(size)
        .filter(|&e| e <= input.len())
        .ok_or(KernelError::ZbootMalformed {
            offset,
            size,
            file: input.len(),
        })?;

    let comp_field = &input[ZBOOT_COMP_OFFSET..ZBOOT_COMP_OFFSET + ZBOOT_COMP_LEN];
    let comp_len = comp_field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(comp_field.len());
    let comp = &comp_field[..comp_len];
    if comp != b"gzip" {
        return Err(KernelError::ZbootCompression(
            String::from_utf8_lossy(comp).into_owned(),
        ));
    }

    let mut image = Vec::new();
    GzDecoder::new(&input[offset..end])
        .read_to_end(&mut image)
        .map_err(|e| KernelError::ZbootDecompress(e.to_string()))?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bzimage() -> Vec<u8> {
        let mut v = vec![0u8; 0x1000];
        v[BZIMAGE_HDRS_OFFSET..BZIMAGE_HDRS_OFFSET + 4]
            .copy_from_slice(&BZIMAGE_HDRS_MAGIC.to_le_bytes());
        v[BZIMAGE_VERSION_OFFSET..BZIMAGE_VERSION_OFFSET + 2]
            .copy_from_slice(&0x020Fu16.to_le_bytes());
        v[BZIMAGE_LOADFLAGS_OFFSET] = 0x01;
        v[BZIMAGE_SETUP_SECTS_OFFSET] = 39;
        v[BZIMAGE_KERNEL_ALIGNMENT_OFFSET..BZIMAGE_KERNEL_ALIGNMENT_OFFSET + 4]
            .copy_from_slice(&0x200000u32.to_le_bytes());
        v[BZIMAGE_INIT_SIZE_OFFSET..BZIMAGE_INIT_SIZE_OFFSET + 4]
            .copy_from_slice(&0x048E5000u32.to_le_bytes());
        v[BZIMAGE_PREF_ADDRESS_OFFSET..BZIMAGE_PREF_ADDRESS_OFFSET + 8]
            .copy_from_slice(&0x0100_0000u64.to_le_bytes());
        v
    }

    fn make_arm64_image() -> Vec<u8> {
        let mut v = vec![0u8; 256];
        v[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + 4]
            .copy_from_slice(&ARM64_IMAGE_MAGIC.to_le_bytes());
        v
    }

    #[test]
    fn detects_bzimage_as_x86_64() {
        let bytes = make_bzimage();
        let p = parse(&bytes).unwrap();
        assert_eq!(p.arch, Arch::X86_64);
        let bz = p.bzimage.unwrap();
        assert_eq!(bz.init_size, 0x048E5000);
        assert_eq!(bz.kernel_alignment, 0x200000);
        assert_eq!(bz.pref_address, 0x0100_0000);
        assert_eq!(bz.setup_sects, 39);
        assert_eq!(bz.setup_bytes(), 40 * 512);
    }

    #[test]
    fn detects_arm64_image() {
        let bytes = make_arm64_image();
        let p = parse(&bytes).unwrap();
        assert_eq!(p.arch, Arch::Aarch64);
        assert!(p.bzimage.is_none());
    }

    fn make_zboot(image: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(image).unwrap();
        let payload = enc.finish().unwrap();
        let offset = ZBOOT_HEADER_MIN; // payload right after the header
        let mut v = vec![0u8; offset];
        v[0..2].copy_from_slice(b"MZ");
        v[ZBOOT_ZIMG_OFFSET..ZBOOT_ZIMG_OFFSET + 4].copy_from_slice(b"zimg");
        v[ZBOOT_PAYLOAD_OFFSET_FIELD..ZBOOT_PAYLOAD_OFFSET_FIELD + 4]
            .copy_from_slice(&(offset as u32).to_le_bytes());
        v[ZBOOT_PAYLOAD_SIZE_FIELD..ZBOOT_PAYLOAD_SIZE_FIELD + 4]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        v[ZBOOT_COMP_OFFSET..ZBOOT_COMP_OFFSET + 4].copy_from_slice(b"gzip");
        v.extend_from_slice(&payload);
        v
    }

    #[test]
    fn unwraps_efi_zboot_to_arm64_image() {
        let img = make_arm64_image();
        let wrapped = make_zboot(&img);
        let raw = unwrap_zboot(wrapped).unwrap();
        assert_eq!(raw, img);
        assert_eq!(parse(&raw).unwrap().arch, Arch::Aarch64);
    }

    #[test]
    fn unwrap_zboot_passes_through_non_zboot() {
        let bz = make_bzimage();
        assert_eq!(unwrap_zboot(bz.clone()).unwrap(), bz);
        let arm = make_arm64_image();
        assert_eq!(unwrap_zboot(arm.clone()).unwrap(), arm);
    }

    #[test]
    fn rejects_non_gzip_zboot() {
        let mut z = make_zboot(&make_arm64_image());
        z[ZBOOT_COMP_OFFSET..ZBOOT_COMP_OFFSET + 4].copy_from_slice(b"zstd");
        assert!(matches!(
            unwrap_zboot(z),
            Err(KernelError::ZbootCompression(_))
        ));
    }

    #[test]
    fn rejects_oversized_kernel_alignment() {
        let mut v = make_bzimage();
        v[BZIMAGE_KERNEL_ALIGNMENT_OFFSET..BZIMAGE_KERNEL_ALIGNMENT_OFFSET + 4]
            .copy_from_slice(&0x2000000u32.to_le_bytes()); // 32 MiB > supported max
        assert!(matches!(
            parse(&v),
            Err(KernelError::KernelAlignmentTooLarge(0x2000000))
        ));
    }

    #[test]
    fn rejects_too_small() {
        let r = parse(&[0u8; 32]);
        assert!(matches!(r, Err(KernelError::TooSmall(_))));
    }

    #[test]
    fn rejects_old_bzimage_protocol() {
        let mut v = make_bzimage();
        v[BZIMAGE_VERSION_OFFSET..BZIMAGE_VERSION_OFFSET + 2]
            .copy_from_slice(&0x0200u16.to_le_bytes());
        assert!(matches!(parse(&v), Err(KernelError::OldProtocol(0x0200))));
    }

    #[test]
    fn rejects_bzimage_without_loaded_high() {
        let mut v = make_bzimage();
        v[BZIMAGE_LOADFLAGS_OFFSET] = 0;
        assert!(matches!(parse(&v), Err(KernelError::NotLoadedHigh)));
    }

    #[test]
    fn rejects_random_bytes() {
        let v = vec![0xAAu8; 1024];
        assert!(matches!(parse(&v), Err(KernelError::Unrecognized)));
    }

    #[test]
    fn pe_machine_codes_match_pmi_spec() {
        assert_eq!(Arch::X86_64.pe_machine(), 0x8664);
        assert_eq!(Arch::Aarch64.pe_machine(), 0xAA64);
    }
}
