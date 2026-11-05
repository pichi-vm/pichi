// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::path::Path;

pub(crate) trait SparseFile {
    fn create_sparse(&self, size: u64) -> File;
}

impl SparseFile for Path {
    fn create_sparse(&self, size: u64) -> File {
        let file = File::create(self).unwrap();
        // Windows requires the sparse flag before extending a file; otherwise
        // the large-image regression fixtures would allocate 65 GiB on disk.
        #[cfg(windows)]
        {
            let output = std::process::Command::new("fsutil")
                .args(["sparse", "setflag"])
                .arg(self)
                .output()
                .unwrap();
            assert!(output.status.success(), "mark sparse fixture: {output:?}");
        }
        file.set_len(size).unwrap();
        file
    }
}
