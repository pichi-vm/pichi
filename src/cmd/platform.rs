// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use conglobate::artifact::Platform;

/// Pichi runs the native guest architecture without CPU emulation.
pub(crate) trait PlatformExt {
    fn host_architecture() -> &'static str;
}
impl PlatformExt for Platform {
    fn host_architecture() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        }
    }
}
