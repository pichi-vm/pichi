// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

fn main() {
    let executable = std::env::current_exe().unwrap();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>().join("\n");
    std::fs::write(executable.parent().unwrap().join("argv.txt"), arguments).unwrap();
    std::fs::write(
        executable.parent().unwrap().join("rust-log.txt"),
        std::env::var("RUST_LOG").unwrap_or_default(),
    )
    .unwrap();
    if let Some(fixture) = std::env::var_os("PICHI_TEST_TERMINAL_FIXTURE") {
        let arguments: Vec<_> = std::env::args().collect();
        let endpoint = &arguments[arguments
            .iter()
            .position(|arg| arg == "--console-resize")
            .unwrap()
            + 1];
        let status = std::process::Command::new(fixture)
            .args([
                "windows_terminal::workload",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("PICHI_TEST_RESIZE_ENDPOINT", endpoint)
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    if std::env::var_os("PICHI_TEST_STORAGE").is_some() {
        use std::io::Write as _;
        let arguments: Vec<_> = std::env::args().collect();
        let spec = &arguments[arguments.iter().position(|arg| arg == "--blk").unwrap() + 1];
        let path = std::path::PathBuf::from(
            spec.split(',')
                .next()
                .unwrap()
                .strip_prefix("path=")
                .unwrap()
                .trim_matches('"'),
        );
        let mut file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let length = file.metadata().unwrap().len();
        file.write_all(b"guest data").unwrap();
        std::fs::write(
            executable.parent().unwrap().join("storage-capacity.txt"),
            length.to_string(),
        )
        .unwrap();
        std::fs::write(
            executable.parent().unwrap().join("storage-path.txt"),
            path.canonicalize().unwrap().to_str().unwrap(),
        )
        .unwrap();
    }
    if std::env::var_os("PICHI_TEST_DILLO_WAIT_FOR_EOF").is_some() {
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    }
    if let Some(gate) = std::env::var_os("PICHI_TEST_DILLO_EXIT_GATE") {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !std::path::Path::new(&gate).exists() {
            assert!(std::time::Instant::now() < deadline, "exit gate deadline");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    if let Ok(code) = std::env::var("PICHI_TEST_DILLO_EXIT") {
        std::process::exit(code.parse().unwrap());
    }
}
