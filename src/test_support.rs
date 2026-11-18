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

/// Run last-owner assertions without unrelated child-process creation.
pub(crate) fn isolated(test: &str) -> bool {
    const CHILD: &str = "PICHI_TEST_ISOLATED_CASE";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    // Fork briefly inherits even close-on-exec lock handles.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .output()
        .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "isolated {test} failed or did not run: {output:?}"
    );
    true
}
