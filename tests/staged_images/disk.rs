// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::common::stages::Image;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

/// Independent inspection of GPT bytes from the actual emitted snapshot chain.
/// This test reader inspects decoded 4 KiB stores from raw or zstd layers.
pub(crate) struct Disk {
    files: Vec<File>,
    chunks: BTreeMap<u64, (usize, u64)>,
}

impl From<&Image> for Disk {
    fn from(image: &Image) -> Self {
        let mut disk = Self {
            files: Vec::new(),
            chunks: BTreeMap::new(),
        };
        for layer in image.layers("scute") {
            let mut file = File::open(image.layer_path(&layer)).unwrap();
            if layer["mediaType"] == "application/vnd.pichi.scute.v1+zstd" {
                let mut decoder = ruzstd::decoding::StreamingDecoder::new(file).unwrap();
                let mut decoded = tempfile::tempfile().unwrap();
                std::io::copy(&mut decoder, &mut decoded).unwrap();
                decoded.rewind().unwrap();
                file = decoded;
            } else {
                assert_eq!(layer["mediaType"], "application/vnd.pichi.scute.v1");
            }
            let length = file.metadata().unwrap().len();
            let mut header = [0u8; 16];
            file.read_exact(&mut header).unwrap();
            assert_eq!(
                header,
                [83, 110, 65, 112, 1, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0]
            );
            let mut area = 0u64;
            'metadata: loop {
                file.seek(SeekFrom::Start((1 + area * 257) * 4096)).unwrap();
                let mut records = [0u8; 4096];
                file.read_exact(&mut records).unwrap();
                for record in records.chunks_exact(16) {
                    let old = u64::from_le_bytes(record[..8].try_into().unwrap());
                    let new = u64::from_le_bytes(record[8..].try_into().unwrap());
                    if new == 0 {
                        break 'metadata;
                    }
                    assert!((new + 1) * 4096 <= length);
                    disk.chunks.insert(old, (disk.files.len(), new * 4096));
                }
                area += 1;
            }
            disk.files.push(file);
        }
        disk
    }
}

impl Disk {
    fn chunk(&mut self, lba: u64) -> [u8; 4096] {
        let mut bytes = [0u8; 4096];
        if let Some(&(index, offset)) = self.chunks.get(&lba) {
            let file = &mut self.files[index];
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.read_exact(&mut bytes).unwrap();
        }
        bytes
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    pub(crate) fn verify(mut self, path: &Path) -> (u64, u64) {
        let primary = self.chunk(1);
        assert_eq!(&primary[..8], b"EFI PART");
        let last = u64::from_le_bytes(primary[32..40].try_into().unwrap());
        assert!((1024..2 * 1024 * 1024).contains(&last));
        // Preserve both tables without materializing several GiB of filesystems.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.set_len((last + 1) * 4096).unwrap();
        for lba in (0..256).chain(last - 255..=last) {
            file.seek(SeekFrom::Start(lba * 4096)).unwrap();
            file.write_all(&self.chunk(lba)).unwrap();
        }
        let size = gpt::disk::LogicalBlockSize::Lb4096;
        let disk = gpt::GptConfig::new()
            .logical_block_size(size)
            .open_from_device(file)
            .unwrap();
        let primary = disk.primary_header().unwrap().clone();
        let backup = disk.backup_header().unwrap().clone();
        assert_eq!(primary.backup_lba, backup.current_lba);
        assert_eq!(backup.backup_lba, primary.current_lba);
        assert_eq!(primary.disk_guid, backup.disk_guid);
        assert!(!primary.disk_guid.is_nil());
        let mut file = disk.take_device();
        let mut tables = Vec::new();
        for header in [&primary, &backup] {
            let bytes = u64::from(header.num_parts) * u64::from(header.part_size);
            assert!(bytes <= 1024 * 1024);
            let mut table = vec![0u8; bytes as usize];
            file.seek(SeekFrom::Start(header.part_start * 4096))
                .unwrap();
            file.read_exact(&mut table).unwrap();
            assert_eq!(Self::crc32(&table), header.crc32_parts);
            tables.push(table);
        }
        assert_eq!(tables[0], tables[1]);
        let partitions = gpt::partition::file_read_partitions(&mut file, &primary, size).unwrap();
        assert_eq!(partitions.len(), 2);
        let native_root = if cfg!(target_arch = "aarch64") {
            gpt::partition_types::LINUX_ROOT_ARM_64
        } else {
            gpt::partition_types::LINUX_ROOT_X64
        };
        let root = partitions
            .values()
            .find(|p| p.part_type_guid == native_root)
            .unwrap();
        let esp = partitions
            .values()
            .find(|p| p.part_type_guid == gpt::partition_types::EFI)
            .unwrap();
        assert_eq!(
            (root.last_lba - root.first_lba + 1) * 4096,
            4 * 1024 * 1024 * 1024
        );
        assert_eq!((esp.last_lba - esp.first_lba + 1) * 4096, 512 * 1024 * 1024);
        assert_eq!(root.first_lba % 256, 0);
        assert_eq!(esp.first_lba % 256, 0);
        assert!(!root.part_guid.is_nil());
        assert_ne!(root.part_guid, esp.part_guid);
        (root.first_lba * 8, (root.last_lba - root.first_lba + 1) * 8)
    }
}
