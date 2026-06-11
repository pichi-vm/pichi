//! Resolve raw 16-byte PARTUUIDs to partition device paths + dev_t,
//! using only kernel-provided information from `/sys/class/block/*/uevent`.
//! No udev required — `udev` populates `/dev/disk/by-partuuid/*` symlinks
//! from this same data, but the data itself comes from the kernel's GPT
//! parser at `losetup --partscan` (or disk attach) time.
//!
//! For each `/sys/class/block/<X>` whose `uevent` declares
//! `DEVTYPE=partition`, the kernel emits `PARTUUID=<textual-uuid>` and
//! `MAJOR=<n>/MINOR=<m>`. [`PartitionMap::scan`] reads them all in one
//! pass into a HashMap so the chain walker can do O(1) lookups instead
//! of paying a full `/sys/class/block` walk per scute (lane 08 HOT-3).

use crate::verity::superblock::VERITY_SUPERBLOCK_SIZE;
use crate::CarapaceError;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;

/// One partition's identity + locator.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedPartition {
    /// Block-device path under `/dev` (e.g. `/dev/loop3p2`,
    /// `/dev/sda7`).
    pub path: PathBuf,
    /// Major:minor for use in dm tables via the `<maj>:<min>` shortcut
    /// (avoids a udev sync round-trip on the consumer side).
    pub major: u32,
    pub minor: u32,
}

impl ResolvedPartition {
    /// `(major, minor)` for use as the `<maj>:<min>` device argument in
    /// a dm-table line. Returns the raw pair so the caller (dm-table
    /// renderer) can format on demand without an intermediate `PathBuf`
    /// allocation. Same shape as `DmDevice::dev_ref`.
    pub fn dev_ref(&self) -> (u32, u32) {
        (self.major, self.minor)
    }
}

/// Pluggable resolver for the chain walker. The default impl is
/// [`PartitionMap`] (sysfs-backed), but the chain walker takes
/// `&dyn ChainResolver` so unit tests can substitute an in-memory
/// fake without touching `/sys/class/block` or doing real disk I/O.
pub(crate) trait ChainResolver {
    /// Look up the cow partition for a scute. Returns just the
    /// location info — there's no parser-validated structure on cow
    /// partitions; dm-verity verifies their content at activation
    /// time.
    fn resolve_cow(&self, partuuid: &[u8; 16]) -> Result<&ResolvedPartition, CarapaceError>;

    /// Look up the verity partition AND read its 4 KiB superblock
    /// area in one call. Tying the I/O to resolution keeps the
    /// walker's I/O surface to a single trait method (and lets the
    /// mock just hand back pre-built bytes).
    fn resolve_verity(
        &self,
        partuuid: &[u8; 16],
    ) -> Result<(&ResolvedPartition, [u8; VERITY_SUPERBLOCK_SIZE]), CarapaceError>;
}

/// All carapace-relevant GPT partitions visible to the kernel,
/// indexed by raw PARTUUID. Built once per attach via [`Self::scan`];
/// the chain walker then does O(1) lookups via [`Self::find`] instead
/// of a full sysfs walk per scute.
#[derive(Debug, Default)]
pub(crate) struct PartitionMap {
    by_partuuid: HashMap<[u8; 16], ResolvedPartition>,
}

impl PartitionMap {
    /// Walk `/sys/class/block` once. Skips entries that are not
    /// partitions, lack a PARTUUID, or have malformed uevents.
    ///
    /// Reuses three buffers (the sysfs path, the dev path, and the
    /// uevent body) across iterations so the per-partition allocation
    /// cost is bounded — the previous shape's `entry.path()`,
    /// `.join("uevent")`, `read_to_string`, and `format!("/dev/{name}")`
    /// allocated a fresh PathBuf/String per iteration even for
    /// non-carapace partitions, dominating attach-time allocator
    /// pressure on systems with many block devices.
    pub fn scan() -> Result<Self, CarapaceError> {
        let mut by_partuuid = HashMap::new();
        let mut sysfs_path = PathBuf::from("/sys/class/block");
        let mut dev_path = PathBuf::from("/dev");
        let mut uevent_buf: Vec<u8> = Vec::with_capacity(512);

        for entry in fs::read_dir("/sys/class/block")? {
            let entry = entry?;
            let name_os = entry.file_name();
            let Some(name) = name_os.to_str() else {
                continue;
            };

            // sysfs_path = /sys/class/block/<name>/uevent (reused).
            sysfs_path.push(name);
            sysfs_path.push("uevent");
            uevent_buf.clear();
            let read_ok = File::open(&sysfs_path).and_then(|mut f| f.read_to_end(&mut uevent_buf));
            sysfs_path.pop(); // remove "uevent"
            sysfs_path.pop(); // remove <name>
            if read_ok.is_err() {
                continue;
            }

            let Ok(uevent) = std::str::from_utf8(&uevent_buf) else {
                continue;
            };
            let Some((partuuid_text, major, minor)) = parse_partition_uevent(uevent) else {
                continue;
            };
            let Some(raw) = textual_partuuid_to_raw(partuuid_text) else {
                continue;
            };

            // Final dev path for the matching partition. The clone
            // gives the HashMap entry an owned PathBuf; the reusable
            // dev_path drops back to "/dev" via pop() for the next
            // iteration. One alloc per matching partition (~5-10),
            // not per visible partition (~125).
            dev_path.push(name);
            let path = dev_path.clone();
            dev_path.pop();

            by_partuuid.insert(raw, ResolvedPartition { path, major, minor });
        }
        Ok(Self { by_partuuid })
    }

    /// Look up a partition by raw 16-byte PARTUUID.
    pub fn find(&self, partuuid: &[u8; 16]) -> Result<&ResolvedPartition, CarapaceError> {
        self.by_partuuid
            .get(partuuid)
            .ok_or_else(|| CarapaceError::PartitionNotFound {
                partuuid: raw_partuuid_to_text(partuuid),
            })
    }
}

impl ChainResolver for PartitionMap {
    fn resolve_cow(&self, partuuid: &[u8; 16]) -> Result<&ResolvedPartition, CarapaceError> {
        self.find(partuuid)
    }

    fn resolve_verity(
        &self,
        partuuid: &[u8; 16],
    ) -> Result<(&ResolvedPartition, [u8; VERITY_SUPERBLOCK_SIZE]), CarapaceError> {
        let part = self.find(partuuid)?;
        // Verity superblock at offset 0 of the verity partition (kernel
        // ABI: veritysetup format --format=1 places it there). The
        // superblock is exactly 512 B; we read just that — saves a 4 KiB
        // stack copy on the resolver return path and 8x the read I/O.
        use std::fs::OpenOptions;
        use std::io::Read;
        let mut f = OpenOptions::new().read(true).open(&part.path)?;
        let mut buf = [0u8; VERITY_SUPERBLOCK_SIZE];
        f.read_exact(&mut buf)?;
        Ok((part, buf))
    }
}

/// Parse `DEVTYPE`/`PARTUUID`/`MAJOR`/`MINOR` from a uevent body.
/// Returns `None` unless DEVTYPE=partition AND all three target fields
/// are present and parseable. Borrows the PARTUUID text out of the
/// passed slice (no allocation).
fn parse_partition_uevent(uevent: &str) -> Option<(&str, u32, u32)> {
    let mut is_partition = false;
    let mut partuuid: Option<&str> = None;
    let mut major: Option<u32> = None;
    let mut minor: Option<u32> = None;
    for line in uevent.lines() {
        if line == "DEVTYPE=partition" {
            is_partition = true;
        } else if let Some(v) = line.strip_prefix("PARTUUID=") {
            partuuid = Some(v);
        } else if let Some(v) = line.strip_prefix("MAJOR=") {
            major = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("MINOR=") {
            minor = v.parse().ok();
        }
    }
    Some((partuuid?, major?, minor?)).filter(|_| is_partition)
}

/// Convert raw 16-byte PARTUUID (mixed-endian, as stored in the GPT)
/// into the textual UUID form the kernel emits in
/// `/sys/class/block/<X>/uevent`'s `PARTUUID` field.
///
/// Mixed-endian: first 3 fields little-endian on disk → reversed in
/// textual form; last 2 big-endian → no swap. Same convention used by
/// every UUID printer on Linux for GPT PARTUUIDs (sgdisk, blkid, udev).
pub(crate) fn raw_partuuid_to_text(raw: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        raw[3], raw[2], raw[1], raw[0],
        raw[5], raw[4],
        raw[7], raw[6],
        raw[8], raw[9],
        raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
    )
}

/// Inverse of [`raw_partuuid_to_text`]. Returns `None` if the input
/// is not a textual UUID (8-4-4-4-12 lowercase hex with dashes).
fn textual_partuuid_to_raw(text: &str) -> Option<[u8; 16]> {
    // Format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx (36 chars, 4 dashes).
    let bytes = text.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let hex_byte = |hi: u8, lo: u8| -> Option<u8> { Some((nibble(hi)? << 4) | nibble(lo)?) };
    // Field byte offsets in the textual form (skipping dashes).
    // 0..8     : field1 (4 LE bytes on disk → reverse)
    // 9..13    : field2 (2 LE bytes on disk → reverse)
    // 14..18   : field3 (2 LE bytes on disk → reverse)
    // 19..23   : field4 (2 BE bytes on disk → no swap)
    // 24..36   : field5 (6 BE bytes on disk → no swap)
    let mut raw = [0u8; 16];
    let f1 = [
        hex_byte(bytes[0], bytes[1])?,
        hex_byte(bytes[2], bytes[3])?,
        hex_byte(bytes[4], bytes[5])?,
        hex_byte(bytes[6], bytes[7])?,
    ];
    raw[0] = f1[3];
    raw[1] = f1[2];
    raw[2] = f1[1];
    raw[3] = f1[0];
    let f2 = [
        hex_byte(bytes[9], bytes[10])?,
        hex_byte(bytes[11], bytes[12])?,
    ];
    raw[4] = f2[1];
    raw[5] = f2[0];
    let f3 = [
        hex_byte(bytes[14], bytes[15])?,
        hex_byte(bytes[16], bytes[17])?,
    ];
    raw[6] = f3[1];
    raw[7] = f3[0];
    raw[8] = hex_byte(bytes[19], bytes[20])?;
    raw[9] = hex_byte(bytes[21], bytes[22])?;
    let mut i = 10;
    let mut p = 24;
    while i < 16 {
        raw[i] = hex_byte(bytes[p], bytes[p + 1])?;
        i += 1;
        p += 2;
    }
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_to_text_round_trips_through_known_vectors() {
        // Two random v4 GUIDs (the carapace-cow and carapace-verity
        // partition type GUIDs from tests/fixtures/build_carapace.sh).
        // Pinned here as the canonical mixed-endian witness — if a
        // future contributor "fixes" the byte order in raw_to_text,
        // these assertions catch it.
        let scute_cow_raw: [u8; 16] = [
            0x11, 0xdd, 0x80, 0x4a, 0xe1, 0xbf, 0x4a, 0xb3, 0x98, 0xc1, 0xf9, 0xf4, 0x8c, 0xee,
            0xdb, 0xf1,
        ];
        assert_eq!(
            raw_partuuid_to_text(&scute_cow_raw),
            "4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1"
        );
        let scute_verity_raw: [u8; 16] = [
            0x40, 0xbb, 0x95, 0x71, 0x29, 0x72, 0x45, 0x47, 0xb5, 0x80, 0x6a, 0x8c, 0xb1, 0x3f,
            0xd7, 0xd1,
        ];
        assert_eq!(
            raw_partuuid_to_text(&scute_verity_raw),
            "7195bb40-7229-4745-b580-6a8cb13fd7d1"
        );
    }

    #[test]
    fn raw_text_round_trip() {
        // Property: text → raw → text is the identity for both pinned
        // GUIDs above and a handful of arbitrary ones.
        for raw in [
            [
                0x11u8, 0xdd, 0x80, 0x4a, 0xe1, 0xbf, 0x4a, 0xb3, 0x98, 0xc1, 0xf9, 0xf4, 0x8c,
                0xee, 0xdb, 0xf1,
            ],
            [0x00u8; 16],
            [0xffu8; 16],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        ] {
            let text = raw_partuuid_to_text(&raw);
            let parsed = textual_partuuid_to_raw(&text).expect("valid uuid text");
            assert_eq!(raw, parsed, "round-trip mismatch for {text}");
        }
    }

    #[test]
    fn unknown_partuuid_is_partition_not_found_error() {
        let map = PartitionMap::scan().unwrap();
        let r = map.find(&[0xFF; 16]);
        assert!(
            matches!(r, Err(CarapaceError::PartitionNotFound { .. })),
            "expected PartitionNotFound, got {r:?}"
        );
    }

    #[test]
    fn parse_partition_uevent_well_formed() {
        let body = "MAJOR=259\nMINOR=3\nDEVNAME=nvme0n1p3\nDEVTYPE=partition\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n";
        let got = parse_partition_uevent(body).unwrap();
        assert_eq!(got, ("4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1", 259, 3));
    }

    #[test]
    fn parse_partition_uevent_rejects_disk() {
        // DEVTYPE=disk (not partition) → None even with all other fields.
        let body =
            "MAJOR=8\nMINOR=0\nDEVTYPE=disk\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n";
        assert!(parse_partition_uevent(body).is_none());
    }

    #[test]
    fn parse_partition_uevent_missing_fields() {
        // Missing PARTUUID.
        assert!(parse_partition_uevent("MAJOR=259\nMINOR=3\nDEVTYPE=partition\n").is_none());
        // Missing MAJOR.
        assert!(parse_partition_uevent(
            "MINOR=3\nDEVTYPE=partition\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n"
        )
        .is_none());
        // Missing DEVTYPE entirely.
        assert!(parse_partition_uevent(
            "MAJOR=259\nMINOR=3\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n"
        )
        .is_none());
        // Unparseable MAJOR.
        assert!(parse_partition_uevent(
            "MAJOR=xx\nMINOR=3\nDEVTYPE=partition\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n"
        )
        .is_none());
    }

    #[test]
    fn rejects_textual_partuuid_with_wrong_shape() {
        assert!(textual_partuuid_to_raw("").is_none());
        assert!(textual_partuuid_to_raw("not-a-uuid").is_none());
        assert!(textual_partuuid_to_raw("4a80dd11_bfe1_b34a_98c1_f9f48ceedbf1").is_none());
        assert!(textual_partuuid_to_raw("ZZZZdd11-bfe1-b34a-98c1-f9f48ceedbf1").is_none());
    }
}
