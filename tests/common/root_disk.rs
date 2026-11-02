// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{Seek as _, SeekFrom};
use std::path::Path;

/// Independent 4 KiB GPT fixture for roots consumed by the real products.
#[derive(Debug)]
pub(super) struct RootDisk;

impl RootDisk {
    pub(super) fn write(filesystem: &Path, destination: &Path) {
        let mut source = File::open(filesystem).unwrap();
        let capacity = source.metadata().unwrap().len();
        assert!(capacity.is_multiple_of(4096));
        let offset = 1024 * 1024;
        let size = offset + capacity + 1024 * 1024;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(destination)
            .unwrap();
        file.set_len(size).unwrap();
        gpt::mbr::ProtectiveMBR::with_lb_size((size / 4096 - 1).try_into().unwrap())
            .overwrite_lba0(&mut file)
            .unwrap();
        let mut table = gpt::GptConfig::new()
            .writable(true)
            .logical_block_size(gpt::disk::LogicalBlockSize::Lb4096)
            .create_from_device(file, None)
            .unwrap();
        table
            .add_partition_at(
                "root",
                1,
                offset / 4096,
                capacity / 4096,
                if cfg!(target_arch = "aarch64") {
                    gpt::partition_types::LINUX_ROOT_ARM_64
                } else {
                    gpt::partition_types::LINUX_ROOT_X64
                },
                0,
            )
            .unwrap();
        let mut file = table.write().unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        assert_eq!(std::io::copy(&mut source, &mut file).unwrap(), capacity);
        file.sync_all().unwrap();
    }
}
