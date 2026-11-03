// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

mod build;
mod console;

use std::{os::unix::process::CommandExt, path::Path};

fn main() -> anyhow::Result<()> {
    let build = console::Console::open(Path::new("/sys"), Path::new("/dev"))?.request()?;
    Err(build.command().exec().into())
}
