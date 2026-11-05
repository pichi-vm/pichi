// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    let executable = std::env::current_exe().unwrap();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>().join("\n");
    std::fs::write(executable.parent().unwrap().join("argv.txt"), arguments).unwrap();
    if let Ok(code) = std::env::var("PICHI_TEST_DILLO_EXIT") {
        std::process::exit(code.parse().unwrap());
    }
}
