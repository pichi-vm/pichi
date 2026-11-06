// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    let executable = std::env::current_exe().unwrap();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>().join("\n");
    std::fs::write(executable.parent().unwrap().join("argv.txt"), arguments).unwrap();
    std::fs::write(
        executable.parent().unwrap().join("rust-log.txt"),
        std::env::var("RUST_LOG").unwrap_or_default(),
    )
    .unwrap();
    if let Some(directory) = std::env::var_os("PICHI_TEST_STORAGE_DIR") {
        use std::io::Write as _;
        let entries: Vec<_> = std::fs::read_dir(directory).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let path = entries[0].as_ref().unwrap().path().join("storage.raw");
        let mut file = std::fs::File::options().read(true).write(true).open(&path).unwrap();
        let length = file.metadata().unwrap().len();
        file.write_all(b"guest data").unwrap();
        std::fs::write(executable.parent().unwrap().join("storage-capacity.txt"), length.to_string()).unwrap();
        std::fs::write(executable.parent().unwrap().join("storage-path.txt"), path.canonicalize().unwrap().to_str().unwrap()).unwrap();
    }
    if let Ok(code) = std::env::var("PICHI_TEST_DILLO_EXIT") {
        std::process::exit(code.parse().unwrap());
    }
}
