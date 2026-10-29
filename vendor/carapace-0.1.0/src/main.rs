// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! carapace CLI — read-side assembler.
//!
//! Single verb: `carapace mount <root-hex> <dir> [--type <fs>]`.
//! Delegates to the `carapace` library; no chain or dm logic here.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod cli;

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    cli::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("carapace: device-mapper assembly requires Linux");
    ExitCode::FAILURE
}
