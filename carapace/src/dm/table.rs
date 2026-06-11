//! dm-table value types + render + the variable-length `DmTableBuf`
//! used to feed `DM_TABLE_LOAD`.
//!
//! `TargetSpec`/`TableLine`/`DmTable` are the operator-facing model;
//! `DmTableBuf` is the kernel-ABI byte buffer (header + per-target
//! `dm_target_spec_raw` + parameter strings) constructed from a
//! `DmTable`. The two layers are kept distinct so the renderer is
//! testable without ioctl plumbing.

use super::header::DmHeader;
use super::uapi::{dm_target_spec_raw, DM_MAX_TYPE_NAME};
use crate::verity::Algorithm;
use crate::CarapaceError;
use zerocopy::{FromBytes, IntoBytes};

#[derive(Debug, Clone)]
pub(crate) enum TargetSpec {
    Linear {
        device: (u32, u32),
        offset_sectors: u64,
    },
    Zero,
    Snapshot {
        origin: (u32, u32),
        cow: (u32, u32),
        chunk_size_sectors: u64,
    },
    /// dm-verity. CRITICAL-1: built only via [`TargetSpec::verity`] which
    /// takes `expected_root` as a separate parameter from `superblock`.
    /// `digest`/`salt` are raw bytes; hex encoding is deferred to
    /// `write_params` so the dm-table renderer can land them directly
    /// in the kernel-ABI buffer without intermediate `String`s.
    Verity {
        data_dev: (u32, u32),
        hash_dev: (u32, u32),
        num_data_blocks: u64,
        algorithm: Algorithm,
        digest: Vec<u8>,
        salt: Vec<u8>,
    },
}

impl TargetSpec {
    /// Construct a verity target from primitive activation params.
    /// CRITICAL-1: `expected_root` is a SEPARATE parameter — there is
    /// NO overload that derives the root from the superblock or salt.
    ///
    /// Takes `algorithm` + `num_data_blocks` + `salt` directly rather
    /// than borrowing a `ValidatedVeritySuperblock`. Decouples the dm
    /// layer from the verity superblock parser — the caller
    /// (`crate::assemble`) is the only place that knows how those
    /// params were validated.
    pub(crate) fn verity(
        data_dev: (u32, u32),
        hash_dev: (u32, u32),
        algorithm: Algorithm,
        num_data_blocks: u64,
        salt: &[u8],
        expected_root: &[u8],
    ) -> Self {
        TargetSpec::Verity {
            data_dev,
            hash_dev,
            num_data_blocks,
            algorithm,
            digest: expected_root.to_vec(),
            salt: salt.to_vec(),
        }
    }

    /// Kernel target-type name (`b"linear"`, `b"zero"`, …) — written
    /// into `dm_target_spec.target_type[16]`.
    fn kernel_type_name(&self) -> &'static [u8] {
        match self {
            Self::Linear { .. } => b"linear",
            Self::Zero => b"zero",
            Self::Snapshot { .. } => b"snapshot",
            Self::Verity { .. } => b"verity",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TableLine {
    pub start: u64,
    pub length: u64,
    pub target: TargetSpec,
}

#[derive(Debug, Clone)]
pub(crate) struct DmTable {
    pub lines: Vec<TableLine>,
}

impl DmTable {
    /// Operator-facing form: `<start> <length> <type> <params>`.
    /// Used as the ERR-04 attachment on `DM_TABLE_LOAD` failures.
    pub(super) fn render_line(&self, idx: usize) -> String {
        let line = &self.lines[idx];
        let type_name = std::str::from_utf8(line.target.kernel_type_name()).unwrap();
        let prefix = format!("{} {} {}", line.start, line.length, type_name);
        let params = self.render_params(idx);
        if params.is_empty() {
            prefix
        } else {
            format!("{prefix} {params}")
        }
    }

    /// Write the per-target kernel-ABI param substring (no
    /// start/length/type) for line `idx` into `w`. Used twice during
    /// `DmTableBuf::build`: once with a counting writer to measure
    /// length, once with a slice writer to land the bytes in the final
    /// buffer — no intermediate `String` per target.
    fn write_params<W: std::fmt::Write>(&self, idx: usize, w: &mut W) -> std::fmt::Result {
        let line = &self.lines[idx];
        match &line.target {
            TargetSpec::Linear {
                device: (maj, min),
                offset_sectors,
            } => write!(w, "{maj}:{min} {offset_sectors}"),
            TargetSpec::Zero => Ok(()),
            TargetSpec::Snapshot {
                origin: (omaj, omin),
                cow: (cmaj, cmin),
                chunk_size_sectors,
            } => write!(
                w,
                // "PO" — persistent with overflow (HIGH-3 lock).
                "{omaj}:{omin} {cmaj}:{cmin} PO {chunk_size_sectors}"
            ),
            TargetSpec::Verity {
                data_dev: (dmaj, dmin),
                hash_dev: (hmaj, hmin),
                num_data_blocks,
                algorithm,
                digest,
                salt,
            } => {
                // RDP-locked: data_block_size == hash_block_size == 4096.
                // Enforced by the verity superblock parser
                // (verity::superblock::RDP_DATA_BLOCK_SIZE et al). The
                // dm-table renderer is downstream of that whitelist —
                // every TargetSpec::Verity built via TargetSpec::verity
                // honors it transitively.
                const RDP_BLOCK_SIZE: u32 = 4096;
                write!(
                    w,
                    "1 {dmaj}:{dmin} {hmaj}:{hmin} {RDP_BLOCK_SIZE} {RDP_BLOCK_SIZE} {num_data_blocks} 1 {} ",
                    algorithm.name(),
                )?;
                crate::util::write_hex_lower(w, digest)?;
                w.write_char(' ')?;
                crate::util::write_hex_lower(w, salt)
            }
        }
    }

    /// Kernel-ABI substring as a `String`. Cold path: only the error
    /// attachment in `render_line` calls this. The hot activation path
    /// uses `write_params` to write directly into the final buffer
    /// without an intermediate allocation.
    pub(super) fn render_params(&self, idx: usize) -> String {
        let mut s = String::new();
        self.write_params(idx, &mut s)
            .expect("String never errs on fmt::Write");
        s
    }

    pub(super) fn render_all(&self) -> String {
        (0..self.lines.len())
            .map(|i| self.render_line(i))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Owned `Vec<u8>` containing a `DmHeader` at the start followed by
/// per-target `dm_target_spec_raw` + parameter strings. Kernel reads
/// `header.data_size` bytes from the pointer iocuddle passes — that
/// equals `bytes.len()` by construction.
///
/// Variable-length safety: iocuddle's typed call thinks it's passing
/// `sizeof(DmHeader) = 312` bytes. The actual buffer is larger. The
/// kernel reads `data_size` bytes (which we set = total length). Sound
/// because:
///   1. `bytes` owns the whole buffer.
///   2. `header_mut()` returns `&mut DmHeader` aliasing only `bytes[..312]`.
///   3. We don't touch `bytes[312..]` while the `&mut DmHeader` is borrowed.
///   4. Kernel sees `data_size` and reads exactly that many bytes.
pub(super) struct DmTableBuf {
    bytes: Vec<u8>,
}

/// `fmt::Write` adapter that counts written bytes without buffering
/// — used in the build() measure pass.
struct CountingWriter(usize);

impl std::fmt::Write for CountingWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// `fmt::Write` adapter that writes into a fixed slot of `&mut [u8]`,
/// advancing the cursor. Used in the build() write pass to land
/// rendered params directly into the final buffer.
struct SliceWriter<'a> {
    bytes: &'a mut [u8],
    pos: usize,
}

impl std::fmt::Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.pos + s.len();
        self.bytes[self.pos..end].copy_from_slice(s.as_bytes());
        self.pos = end;
        Ok(())
    }
}

impl DmTableBuf {
    /// Build the `DM_TABLE_LOAD` payload for `name` from `table`.
    /// CRITICAL-10: always allocates fresh; never reuses buffers.
    ///
    /// Each target is laid out as:
    ///   `dm_target_spec_raw (40)` + `params bytes` + `\0` + zero pad
    /// rounded up to 8-byte alignment. `dm_target_spec.next` = aligned
    /// length (offset to next spec); 0 for the last spec.
    ///
    /// Two passes over the targets — first counts param bytes via
    /// `CountingWriter`, second writes params directly into the final
    /// buffer via `SliceWriter`. Avoids the per-target intermediate
    /// `String` and a `Vec<(usize, String, usize)>` the prior shape
    /// allocated.
    pub(super) fn build(name: &str, table: &DmTable) -> Result<Self, CarapaceError> {
        let header_size = DmHeader::SIZE;
        let spec_size = core::mem::size_of::<dm_target_spec_raw>();

        // Measure pass: aligned slot length per target.
        let mut aligned_lens: Vec<usize> = Vec::with_capacity(table.lines.len());
        let mut payload_len = 0usize;
        for i in 0..table.lines.len() {
            let mut counter = CountingWriter(0);
            table
                .write_params(i, &mut counter)
                .expect("CountingWriter is infallible");
            let aligned = (spec_size + counter.0 + 1).next_multiple_of(8);
            aligned_lens.push(aligned);
            payload_len += aligned;
        }
        let total_len = header_size + payload_len;

        // Allocate fresh + zero-initialized.
        let mut bytes = vec![0u8; total_len];

        // Header: build via the safe newtype, then write its bytes via
        // zerocopy::IntoBytes — no unsafe cast.
        let mut header = DmHeader::new(name)?;
        header.set_data_size(total_len as u32);
        header.set_target_count(table.lines.len() as u32);
        bytes[..header_size].copy_from_slice(header.as_bytes());

        // Write pass: per-target spec header + params written directly
        // into `bytes`. NUL terminator + trailing padding remain zero
        // from the initial vec![0u8; total_len].
        let mut abs_offset = header_size;
        let last_idx = table.lines.len() - 1;
        for (i, line) in table.lines.iter().enumerate() {
            let aligned_len = aligned_lens[i];
            let next_offset = if i == last_idx { 0 } else { aligned_len as u32 };
            let mut target_type = [0u8; DM_MAX_TYPE_NAME];
            let tn = line.target.kernel_type_name();
            target_type[..tn.len()].copy_from_slice(tn);

            let spec = dm_target_spec_raw {
                sector_start: line.start,
                length: line.length,
                status: 0,
                next: next_offset,
                target_type,
            };
            bytes[abs_offset..abs_offset + spec_size].copy_from_slice(spec.as_bytes());

            let param_offset = abs_offset + spec_size;
            let param_slot_end = abs_offset + aligned_len - 1; // -1 reserves the NUL byte
            let mut writer = SliceWriter {
                bytes: &mut bytes[param_offset..param_slot_end],
                pos: 0,
            };
            table
                .write_params(i, &mut writer)
                .expect("SliceWriter is infallible: slot was sized in the measure pass");

            abs_offset += aligned_len;
        }

        Ok(Self { bytes })
    }

    /// `&mut DmHeader` at the start of the buffer. Use to call ioctl.
    /// Safe: zerocopy guarantees `bytes[..312]` is a valid `DmHeader`
    /// (we wrote it there via `DmHeader::as_bytes()` in `build`).
    pub(super) fn header_mut(&mut self) -> &mut DmHeader {
        let (h, _) = DmHeader::mut_from_prefix(&mut self.bytes)
            .expect("DmTableBuf invariant: bytes[..312] is a valid DmHeader");
        h
    }

    /// Buffer length (= `header.data_size`). Used by layout tests; not
    /// needed in production paths. (`is_empty` is intentionally absent
    /// — `build` always produces ≥ `DmHeader::SIZE` bytes.)
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Test-only byte accessor for layout assertions (e.g., that the
    /// per-target `next` chain field is set correctly across multiple
    /// targets). Production code never reads the buffer back.
    #[cfg(test)]
    pub(super) fn raw_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Tests construct verity superblock fixtures from raw bytes; the
    // production code in this file is decoupled from the parser.
    use crate::verity::ValidatedVeritySuperblock;

    #[test]
    fn sizeof_dm_target_spec_raw() {
        assert_eq!(core::mem::size_of::<dm_target_spec_raw>(), 40);
    }

    #[test]
    fn render_linear_operator_form() {
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 1024,
                target: TargetSpec::Linear {
                    device: (252, 5),
                    offset_sectors: 5,
                },
            }],
        };
        assert_eq!(t.render_line(0), "0 1024 linear 252:5 5");
        assert_eq!(t.render_params(0), "252:5 5");
    }

    #[test]
    fn render_zero_kernel_abi_is_empty() {
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 8,
                target: TargetSpec::Zero,
            }],
        };
        assert_eq!(t.render_line(0), "0 8 zero");
        assert_eq!(t.render_params(0), "");
    }

    #[test]
    fn snapshot_renders_with_po_persistence() {
        // HIGH-3 lock witness — the rendered table line MUST contain
        // " PO " (persistent + overflow). dm-snapshot has many tokens;
        // only PO is correct for production read stacks.
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 1024,
                target: TargetSpec::Snapshot {
                    origin: (252, 1),
                    cow: (252, 2),
                    chunk_size_sectors: 8,
                },
            }],
        };
        assert_eq!(t.render_line(0), "0 1024 snapshot 252:1 252:2 PO 8");
    }

    /// CRITICAL-1 witness: TargetSpec::verity uses `expected_root`,
    /// not the superblock's UUID/data, for the `digest_hex` token.
    /// Also covers the verity table-line shape per kernel docs:
    /// `1 <data> <hash> 4096 4096 <num_blocks> 1 <alg> <root> <salt>`.
    #[test]
    fn verity_ctor_uses_expected_root_and_renders_per_kernel_docs() {
        use crate::verity::superblock::VERITY_SUPERBLOCK_SIZE;
        let mut buf = [0u8; VERITY_SUPERBLOCK_SIZE];
        buf[..8].copy_from_slice(b"verity\0\0");
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        buf[12..16].copy_from_slice(&1u32.to_le_bytes());
        buf[32..38].copy_from_slice(b"sha256");
        buf[64..68].copy_from_slice(&4096u32.to_le_bytes());
        buf[68..72].copy_from_slice(&4096u32.to_le_bytes());
        buf[72..80].copy_from_slice(&10u64.to_le_bytes());
        buf[80..82].copy_from_slice(&32u16.to_le_bytes());
        for b in buf[88..120].iter_mut() {
            *b = 0xAA;
        }
        let sb = ValidatedVeritySuperblock::parse(&buf, 0).unwrap();
        let target = TargetSpec::verity(
            (252, 100),
            (252, 101),
            sb.algorithm,
            sb.data_blocks,
            sb.full_salt(),
            &[0xBB; 32],
        );
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 80,
                target,
            }],
        };
        let line = t.render_line(0);
        // Shape witness: literal token order and constants per kernel docs.
        assert!(line.contains("verity 1 252:100 252:101 4096 4096 10 1 sha256"));
        // CRITICAL-1: digest is the expected_root (0xBB..), salt is the
        // superblock's salt bytes (0xAA..). Asserted via the last two
        // whitespace-separated tokens of the rendered line.
        let toks: Vec<&str> = line.split_whitespace().collect();
        let salt_tok = *toks.last().unwrap();
        let digest_tok = toks[toks.len() - 2];
        assert_eq!(digest_tok, "bb".repeat(32));
        assert_eq!(salt_tok, "aa".repeat(32));
    }

    #[test]
    fn buf_for_zero_target_has_correct_layout() {
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 8,
                target: TargetSpec::Zero,
            }],
        };
        let mut buf = DmTableBuf::build("test", &t).unwrap();
        // 312 header + (40 spec + 0 params + 1 NUL = 41 -> padded to 48).
        assert_eq!(buf.len(), 312 + 48);
        let header = buf.header_mut();
        // Header was constructed via DmHeader::new — version is [4,0,0].
        assert_eq!(header.major_version(), 4);
    }

    #[test]
    fn buf_for_linear_target_has_correct_layout() {
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 1024,
                target: TargetSpec::Linear {
                    device: (252, 5),
                    offset_sectors: 0,
                },
            }],
        };
        let buf = DmTableBuf::build("test", &t).unwrap();
        let aligned = (40 + "252:5 0".len() + 1).next_multiple_of(8);
        assert_eq!(buf.len(), 312 + aligned);
    }

    #[test]
    fn buf_for_snapshot_target_has_correct_layout_and_params() {
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 1024,
                target: TargetSpec::Snapshot {
                    origin: (252, 1),
                    cow: (252, 2),
                    chunk_size_sectors: 8,
                },
            }],
        };
        let buf = DmTableBuf::build("test", &t).unwrap();
        let params = "252:1 252:2 PO 8";
        let aligned = (40 + params.len() + 1).next_multiple_of(8);
        assert_eq!(buf.len(), 312 + aligned);
        // Witness the param string is in the buffer at the expected offset.
        let bytes = buf.raw_bytes();
        let param_start = 312 + 40;
        assert_eq!(
            &bytes[param_start..param_start + params.len()],
            params.as_bytes()
        );
        assert_eq!(bytes[param_start + params.len()], 0); // NUL terminator
    }

    #[test]
    fn buf_for_verity_target_has_correct_layout_and_params() {
        use crate::verity::superblock::VERITY_SUPERBLOCK_SIZE;
        let mut sb_buf = [0u8; VERITY_SUPERBLOCK_SIZE];
        sb_buf[..8].copy_from_slice(b"verity\0\0");
        sb_buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        sb_buf[12..16].copy_from_slice(&1u32.to_le_bytes());
        sb_buf[32..38].copy_from_slice(b"sha256");
        sb_buf[64..68].copy_from_slice(&4096u32.to_le_bytes());
        sb_buf[68..72].copy_from_slice(&4096u32.to_le_bytes());
        sb_buf[72..80].copy_from_slice(&7u64.to_le_bytes());
        sb_buf[80..82].copy_from_slice(&32u16.to_le_bytes());
        for b in sb_buf[88..120].iter_mut() {
            *b = 0x55;
        }
        let sb = ValidatedVeritySuperblock::parse(&sb_buf, 0).unwrap();
        let t = DmTable {
            lines: vec![TableLine {
                start: 0,
                length: 56,
                target: TargetSpec::verity(
                    (253, 3),
                    (253, 4),
                    sb.algorithm,
                    sb.data_blocks,
                    sb.full_salt(),
                    &[0xCD; 32],
                ),
            }],
        };
        let buf = DmTableBuf::build("test", &t).unwrap();
        // Reconstruct the expected params to compute alignment + assert
        // the bytes landed verbatim.
        let cd_hex = "cd".repeat(32);
        let salt_hex = "55".repeat(32);
        let params = format!("1 253:3 253:4 4096 4096 7 1 sha256 {cd_hex} {salt_hex}");
        let aligned = (40 + params.len() + 1).next_multiple_of(8);
        assert_eq!(buf.len(), 312 + aligned);
        let bytes = buf.raw_bytes();
        let param_start = 312 + 40;
        assert_eq!(
            &bytes[param_start..param_start + params.len()],
            params.as_bytes()
        );
        assert_eq!(bytes[param_start + params.len()], 0);
    }

    #[test]
    fn buf_for_two_target_table_chains_specs_with_aligned_offsets() {
        // The single-target tests can't catch a bug in next_offset
        // because the only spec has next = 0. This test exercises the
        // chain: first spec's `next` MUST be the aligned length of its
        // own (spec + params + NUL), second spec's `next` MUST be 0.
        let t = DmTable {
            lines: vec![
                TableLine {
                    start: 0,
                    length: 8,
                    target: TargetSpec::Zero,
                },
                TableLine {
                    start: 8,
                    length: 1024,
                    target: TargetSpec::Linear {
                        device: (252, 5),
                        offset_sectors: 5,
                    },
                },
            ],
        };
        let buf = DmTableBuf::build("test", &t).unwrap();

        // Layout: header (312) + zero-spec (40 + 0 + 1 = 41 → 48)
        //                       + linear-spec (40 + "252:5 5".len() + 1 → aligned).
        // Zero target params are empty: spec_size (40) + 0 params + 1 NUL.
        let zero_aligned = (40usize + 1).next_multiple_of(8);
        assert_eq!(zero_aligned, 48);
        let linear_aligned = (40 + "252:5 5".len() + 1).next_multiple_of(8);
        assert_eq!(buf.len(), 312 + zero_aligned + linear_aligned);

        let bytes = buf.raw_bytes();

        // dm_target_spec_raw layout (LE):
        //   sector_start u64    : offset 0..8
        //   length      u64     : offset 8..16
        //   status      i32     : offset 16..20
        //   next        u32     : offset 20..24
        //   target_type [u8;16] : offset 24..40
        let spec0 = 312;
        let spec1 = 312 + zero_aligned;

        let next0 = u32::from_le_bytes(bytes[spec0 + 20..spec0 + 24].try_into().unwrap());
        assert_eq!(
            next0, zero_aligned as u32,
            "spec[0].next must equal aligned size of spec[0]"
        );

        let next1 = u32::from_le_bytes(bytes[spec1 + 20..spec1 + 24].try_into().unwrap());
        assert_eq!(next1, 0, "last spec's next must be 0");

        // Witness target_type strings landed in both specs.
        let type0 = &bytes[spec0 + 24..spec0 + 24 + 4]; // "zero"
        assert_eq!(type0, b"zero");
        let type1 = &bytes[spec1 + 24..spec1 + 24 + 6]; // "linear"
        assert_eq!(type1, b"linear");

        // Witness sector_start of spec[1] (=8 from the push above).
        let start1 = u64::from_le_bytes(bytes[spec1..spec1 + 8].try_into().unwrap());
        assert_eq!(start1, 8);
    }
}
