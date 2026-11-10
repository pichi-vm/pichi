// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_os = "linux")]
mod console;
#[cfg(target_os = "linux")]
mod launch;
#[cfg(target_os = "linux")]
mod response;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::path::Path;
    let mut port = console::open_build_console(Path::new("/sys"), Path::new("/dev"))?;
    console::write_build_input_request(&mut port)?;
    let result = response::read_port(&mut port).and_then(|input| launch::run(&input));
    let reported = console::write_build_result(
        &mut port,
        result
            .as_ref()
            .copied()
            .map_err(|error| format!("{error:#}")),
    );
    // Keep the original build error if reporting also fails.
    result?;
    reported?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pichi-builder requires a Linux build appliance");
    std::process::exit(1);
}
