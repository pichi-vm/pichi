//! Initrd input handling.
//!
//! Accepts exactly two input shapes:
//!
//! 1. A cpio newc archive — passed through unchanged.
//! 2. A statically-linked ELF executable — wrapped in a single-entry
//!    cpio newc archive containing `/`, `/dev`, `/dev/console`, `/init`
//!    and `TRAILER!!!`.
//!
//! Any other input — including dynamically-linked ELF binaries — is
//! rejected. The kernel's PID 1 (`/init`) cannot resolve a dynamic
//! interpreter from an initramfs that ships only the executable, so
//! quietly wrapping such a file would produce a PMI that triple-faults
//! at guest boot. Surfacing the error at arma time makes the failure
//! mode legible.
//!
//! Detection:
//!   - cpio newc: first 6 bytes are the magic `070701`.
//!   - ELF: first 4 bytes are `\x7fELF`.
//!   - "static" means the ELF has no `PT_INTERP` program header.

use std::borrow::Cow;

use goblin::elf::Elf;
use goblin::elf::program_header::PT_INTERP;
use thiserror::Error;

/// cpio newc magic prefix.
const NEWC_MAGIC: &[u8; 6] = b"070701";

/// ELF magic prefix.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

/// Errors from initrd materialization.
#[derive(Debug, Error)]
pub(crate) enum InitrdError {
    /// The input binary is too large to be representable in the
    /// cpio newc header (which uses 8 hex digits — 32 bits — for
    /// the filesize field).
    #[error(
        "init binary is {0} bytes; cpio newc filesize field is a 32-bit (8-hex-digit) value, max {max} bytes",
        max = u32::MAX as u64
    )]
    InitTooLarge(usize),

    /// The input is neither a cpio newc archive nor an ELF binary.
    #[error(
        "initrd input is neither a cpio newc archive (magic `070701`) nor an ELF binary (magic `\\x7fELF`)"
    )]
    UnsupportedFormat,

    /// The input is an ELF but failed to parse.
    #[error("failed to parse ELF initrd input")]
    ElfParse(#[from] goblin::error::Error),

    /// The input is an ELF but is dynamically linked (has a
    /// `PT_INTERP` program header naming a dynamic loader).
    #[error(
        "ELF init binary is dynamically linked (PT_INTERP = `{0}`); arma requires a static binary because the initramfs contains only the executable"
    )]
    DynamicallyLinked(String),
}

/// Determine whether `bytes` is already a cpio newc archive.
pub(crate) fn is_cpio(bytes: &[u8]) -> bool {
    bytes.len() >= NEWC_MAGIC.len() && &bytes[..NEWC_MAGIC.len()] == NEWC_MAGIC
}

/// Determine whether `bytes` starts with the ELF magic.
fn is_elf(bytes: &[u8]) -> bool {
    bytes.len() >= ELF_MAGIC.len() && &bytes[..ELF_MAGIC.len()] == ELF_MAGIC
}

/// Verify that an ELF has no `PT_INTERP` program header. Returns the
/// interpreter path on failure so the error message names the
/// dependency the user would have to satisfy.
fn check_static_elf(bytes: &[u8]) -> Result<(), InitrdError> {
    let elf = Elf::parse(bytes)?;
    for ph in &elf.program_headers {
        if ph.p_type == PT_INTERP {
            let interp = elf.interpreter.unwrap_or("<unknown>").to_string();
            return Err(InitrdError::DynamicallyLinked(interp));
        }
    }
    Ok(())
}

/// Materialize an initramfs from the raw input. If the input is
/// already cpio, the original slice is returned borrowed (no copy —
/// large initrds can be many MiB). If the input is a static ELF, it
/// is wrapped in a single-entry cpio archive at `/init`. Any other
/// input is rejected.
///
/// A caller-supplied cpio archive is passed through unchecked — its
/// internal validity is the caller's responsibility.
pub(crate) fn materialize(input: &[u8]) -> Result<Cow<'_, [u8]>, InitrdError> {
    if is_cpio(input) {
        return Ok(Cow::Borrowed(input));
    }
    if is_elf(input) {
        check_static_elf(input)?;
        if input.len() > u32::MAX as usize {
            return Err(InitrdError::InitTooLarge(input.len()));
        }
        return Ok(Cow::Owned(wrap_as_cpio(input)));
    }
    Err(InitrdError::UnsupportedFormat)
}

/// Emit a cpio newc entry with the given inode, mode, link count,
/// name, and data. Optionally a `(major, minor)` for character/block
/// device nodes.
fn cpio_entry(
    out: &mut Vec<u8>,
    ino: u32,
    mode: u32,
    nlink: u32,
    name: &str,
    data: &[u8],
    rdev: Option<(u32, u32)>,
) {
    let namesize = name.len() + 1; // includes NUL terminator
    let (rdevmajor, rdevminor) = rdev.unwrap_or((0, 0));
    let header = format!(
        "070701\
         {ino:08X}\
         {mode:08X}\
         {uid:08X}\
         {gid:08X}\
         {nlink:08X}\
         {mtime:08X}\
         {filesize:08X}\
         {devmajor:08X}\
         {devminor:08X}\
         {rdevmajor:08X}\
         {rdevminor:08X}\
         {namesize:08X}\
         {check:08X}",
        uid = 0,
        gid = 0,
        mtime = 0,
        filesize = data.len(),
        devmajor = 0,
        devminor = 0,
        check = 0,
    );

    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0); // NUL
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Wrap a single executable in a minimal cpio newc archive.
///
/// Layout:
///   /                    (dir, nlink=3)
///   /dev                 (dir, nlink=2)
///   /dev/console         (char device, major 5 minor 1, rw-rw-rw-)
///   /init                (executable, 0755, the input bytes)
///   TRAILER!!!           (sentinel)
fn wrap_as_cpio(init_binary: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(512 + init_binary.len());
    cpio_entry(&mut out, 1, 0o040_755, 3, ".", &[], None);
    cpio_entry(&mut out, 2, 0o040_755, 2, "dev", &[], None);
    cpio_entry(
        &mut out,
        3,
        0o020_666, // char device, rw-rw-rw-
        1,
        "dev/console",
        &[],
        Some((5, 1)),
    );
    cpio_entry(&mut out, 4, 0o100_755, 1, "init", init_binary, None);
    cpio_entry(&mut out, 0, 0, 1, "TRAILER!!!", &[], None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ELF64 little-endian header with the given
    /// program-header table. Used by tests to synthesize static and
    /// dynamically-linked inputs without shelling out to a compiler.
    fn synth_elf(program_headers: &[(u32, &[u8])]) -> Vec<u8> {
        // ELF64 header is 64 bytes; each PHDR is 56 bytes.
        const EHDR_SIZE: usize = 64;
        const PHDR_SIZE: usize = 56;
        let phnum = program_headers.len();
        let phoff = EHDR_SIZE;
        // Lay out PT_INTERP payloads after the program-header table.
        let mut payload_offset = phoff + phnum * PHDR_SIZE;
        let mut payloads: Vec<(usize, &[u8])> = Vec::new();
        for (_, payload) in program_headers {
            payloads.push((payload_offset, payload));
            payload_offset += payload.len();
        }
        let total_size = payload_offset;

        let mut out = vec![0u8; total_size];
        // e_ident
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // ELFDATA2LSB
        out[6] = 1; // EV_CURRENT
        // e_type = ET_EXEC (2)
        out[16..18].copy_from_slice(&2u16.to_le_bytes());
        // e_machine = EM_X86_64 (0x3E)
        out[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        // e_version = 1
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        // e_entry
        out[24..32].copy_from_slice(&0u64.to_le_bytes());
        // e_phoff
        out[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        // e_shoff = 0
        out[40..48].copy_from_slice(&0u64.to_le_bytes());
        // e_flags = 0
        out[48..52].copy_from_slice(&0u32.to_le_bytes());
        // e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx
        out[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
        out[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        out[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        out[58..60].copy_from_slice(&0u16.to_le_bytes());
        out[60..62].copy_from_slice(&0u16.to_le_bytes());
        out[62..64].copy_from_slice(&0u16.to_le_bytes());

        // Program headers
        for (i, (p_type, payload)) in program_headers.iter().enumerate() {
            let base = phoff + i * PHDR_SIZE;
            let (poff, _) = payloads[i];
            out[base..base + 4].copy_from_slice(&p_type.to_le_bytes());
            out[base + 4..base + 8].copy_from_slice(&0u32.to_le_bytes()); // p_flags
            out[base + 8..base + 16].copy_from_slice(&(poff as u64).to_le_bytes()); // p_offset
            out[base + 16..base + 24].copy_from_slice(&0u64.to_le_bytes()); // p_vaddr
            out[base + 24..base + 32].copy_from_slice(&0u64.to_le_bytes()); // p_paddr
            out[base + 32..base + 40].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // p_filesz
            out[base + 40..base + 48].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // p_memsz
            out[base + 48..base + 56].copy_from_slice(&1u64.to_le_bytes()); // p_align
        }

        // Payloads
        for (offset, payload) in payloads {
            out[offset..offset + payload.len()].copy_from_slice(payload);
        }

        out
    }

    #[test]
    fn detects_cpio_by_magic() {
        assert!(is_cpio(b"070701ABCDEF"));
        assert!(!is_cpio(b"\x7fELF"));
        assert!(!is_cpio(b""));
    }

    #[test]
    fn detects_elf_by_magic() {
        assert!(is_elf(b"\x7fELFmore"));
        assert!(!is_elf(b"070701"));
        assert!(!is_elf(b"\x7fEL"));
        assert!(!is_elf(b""));
    }

    #[test]
    fn materialize_passes_cpio_through_unchanged() {
        let cpio = b"070701FAKEDATA".to_vec();
        let out = materialize(&cpio).unwrap();
        assert_eq!(out.as_ref(), cpio.as_slice());
    }

    #[test]
    fn materialize_borrows_cpio_input_without_copying() {
        let cpio = b"070701PASSTHROUGH".to_vec();
        let out = materialize(&cpio).unwrap();
        // The pass-through path MUST be Borrowed — for multi-MiB
        // initrds this avoids an extra full-buffer copy.
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn materialize_wraps_static_elf() {
        // PT_LOAD = 1; no PT_INTERP → static.
        let elf = synth_elf(&[(1, b"payload")]);
        let out = materialize(&elf).unwrap();
        assert!(matches!(out, Cow::Owned(_)));
        assert!(is_cpio(&out));
        assert!(out.windows(b"TRAILER!!!".len()).any(|w| w == b"TRAILER!!!"));
        assert!(
            out.windows(b"dev/console".len())
                .any(|w| w == b"dev/console")
        );
    }

    #[test]
    fn materialize_rejects_dynamic_elf() {
        // PT_INTERP = 3, payload is the interpreter path as a
        // NUL-terminated C string.
        let interp = b"/lib64/ld-linux-x86-64.so.2\0";
        let elf = synth_elf(&[(3, interp)]);
        let err = materialize(&elf).unwrap_err();
        match err {
            InitrdError::DynamicallyLinked(path) => {
                assert_eq!(path, "/lib64/ld-linux-x86-64.so.2");
            }
            other => panic!("expected DynamicallyLinked, got {other:?}"),
        }
    }

    #[test]
    fn materialize_rejects_arbitrary_bytes() {
        // Not cpio, not ELF.
        let err = materialize(b"this is not an init binary").unwrap_err();
        assert!(matches!(err, InitrdError::UnsupportedFormat));
    }

    #[test]
    fn materialize_rejects_empty_input() {
        let err = materialize(b"").unwrap_err();
        assert!(matches!(err, InitrdError::UnsupportedFormat));
    }

    #[test]
    fn materialize_rejects_truncated_elf_header() {
        // Has ELF magic but truncated before the rest of the header
        // can be parsed.
        let err = materialize(b"\x7fELF").unwrap_err();
        // Goblin returns a parse error; we just confirm it's not one
        // of the "accept" branches.
        assert!(matches!(err, InitrdError::ElfParse(_)));
    }
}
