// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Resolve raw 16-byte PARTUUIDs to open partition files,
//! using only kernel-provided information from `/sys/class/block/*/uevent`.
//! No udev required — `udev` populates `/dev/disk/by-partuuid/*` symlinks
//! from this same data, but the data itself comes from the kernel's GPT
//! parser at `losetup --partscan` (or disk attach) time.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::PathBuf;

/// One partition's identity + locator.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedPartition {
    /// Block-device path under `/dev` (e.g. `/dev/loop3p2`, `/dev/sda7`).
    pub path: PathBuf,
}

/// Pluggable resolver for the chain walker. The default impl is
/// [`PartitionMap`] (sysfs-backed), but the chain walker takes
/// `&dyn ChainResolver` so unit tests can substitute an in-memory
/// fake backed by temporary files without touching `/sys/class/block`.
pub(crate) trait ChainResolver {
    fn resolve_data(&self, partuuid: &[u8; 16]) -> io::Result<File>;
    fn resolve_hash(&self, partuuid: &[u8; 16]) -> io::Result<File>;
}

/// All carapace-relevant GPT partitions visible to the kernel,
/// indexed by raw PARTUUID.
#[derive(Debug, Default)]
pub(crate) struct PartitionMap {
    by_partuuid: HashMap<[u8; 16], ResolvedPartition>,
}

impl PartitionMap {
    /// Walk `/sys/class/block` once.
    pub(crate) fn scan() -> io::Result<Self> {
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

            sysfs_path.push(name);
            sysfs_path.push("uevent");
            uevent_buf.clear();
            let read_ok = File::open(&sysfs_path).and_then(|mut f| f.read_to_end(&mut uevent_buf));
            sysfs_path.pop();
            sysfs_path.pop();
            if read_ok.is_err() {
                continue;
            }

            let Ok(uevent) = std::str::from_utf8(&uevent_buf) else {
                continue;
            };
            // Check it's a partition with a PARTUUID; major/minor not stored.
            let Some((partuuid_text, _major, _minor)) = parse_partition_uevent(uevent) else {
                continue;
            };
            let Some(raw) = textual_partuuid_to_raw(partuuid_text) else {
                continue;
            };

            dev_path.push(name);
            let path = dev_path.clone();
            dev_path.pop();

            by_partuuid.insert(raw, ResolvedPartition { path });
        }
        Ok(Self { by_partuuid })
    }

    /// Look up a partition by raw 16-byte PARTUUID.
    pub(crate) fn find(&self, partuuid: &[u8; 16]) -> io::Result<&ResolvedPartition> {
        self.by_partuuid.get(partuuid).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "partition not found: PARTUUID {}",
                    raw_partuuid_to_text(partuuid)
                ),
            )
        })
    }
}

impl ChainResolver for PartitionMap {
    fn resolve_data(&self, partuuid: &[u8; 16]) -> io::Result<File> {
        File::open(&self.find(partuuid)?.path)
    }

    fn resolve_hash(&self, partuuid: &[u8; 16]) -> io::Result<File> {
        File::open(&self.find(partuuid)?.path)
    }
}

/// Parse `DEVTYPE`/`PARTUUID`/`MAJOR`/`MINOR` from a uevent body.
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
    is_partition.then_some((partuuid?, major?, minor?))
}

/// Convert raw 16-byte PARTUUID (mixed-endian, as stored in the GPT)
/// into the textual UUID form the kernel emits.
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

/// Inverse of [`raw_partuuid_to_text`].
fn textual_partuuid_to_raw(text: &str) -> Option<[u8; 16]> {
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
        let scute_data_raw: [u8; 16] = [
            0x11, 0xdd, 0x80, 0x4a, 0xe1, 0xbf, 0x4a, 0xb3, 0x98, 0xc1, 0xf9, 0xf4, 0x8c, 0xee,
            0xdb, 0xf1,
        ];
        assert_eq!(
            raw_partuuid_to_text(&scute_data_raw),
            "4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1"
        );
        let scute_hash_raw: [u8; 16] = [
            0x40, 0xbb, 0x95, 0x71, 0x29, 0x72, 0x45, 0x47, 0xb5, 0x80, 0x6a, 0x8c, 0xb1, 0x3f,
            0xd7, 0xd1,
        ];
        assert_eq!(
            raw_partuuid_to_text(&scute_hash_raw),
            "7195bb40-7229-4745-b580-6a8cb13fd7d1"
        );
    }

    #[test]
    fn raw_text_round_trip() {
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
    fn unknown_partuuid_is_not_found_error() {
        let map = PartitionMap::scan().unwrap();
        let r = map.find(&[0xFF; 16]);
        assert!(
            matches!(r, Err(ref e) if e.kind() == io::ErrorKind::NotFound),
            "expected NotFound, got {r:?}"
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
        let body =
            "MAJOR=8\nMINOR=0\nDEVTYPE=disk\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n";
        assert!(parse_partition_uevent(body).is_none());
    }

    #[test]
    fn parse_partition_uevent_missing_fields() {
        assert!(parse_partition_uevent("MAJOR=259\nMINOR=3\nDEVTYPE=partition\n").is_none());
        assert!(parse_partition_uevent(
            "MINOR=3\nDEVTYPE=partition\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n"
        )
        .is_none());
        assert!(parse_partition_uevent(
            "MAJOR=259\nMINOR=3\nPARTUUID=4a80dd11-bfe1-b34a-98c1-f9f48ceedbf1\n"
        )
        .is_none());
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
