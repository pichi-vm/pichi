// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_os = "linux")]
mod console;

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        console::Console::open(std::path::Path::new("/sys"), std::path::Path::new("/dev"))?
            .run(std::process::Command::new("conglobate"))
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("pichi-guest requires Linux")
}
