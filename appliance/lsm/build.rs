// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

fn main() -> aya_build::Result<()> {
    aya_build::build_ebpf(
        [aya_build::Package {
            name: "conglobate-label-policy",
            root_dir: "program",
            ..Default::default()
        }],
        aya_build::Toolchain::Nightly,
    )
}
