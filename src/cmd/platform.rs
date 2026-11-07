// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Result, bail, ensure};
use conglobate::artifact::Platform;
use object::{Object as _, read::pe::PeFile64};

/// Pichi runs the native guest architecture without CPU emulation.
pub(crate) trait PlatformExt {
    fn host_architecture() -> &'static str;
    fn from_pmi(bytes: &[u8], explicit: Option<&str>) -> Result<Self>
    where
        Self: Sized;
}
impl PlatformExt for Platform {
    fn from_pmi(bytes: &[u8], explicit: Option<&str>) -> Result<Self> {
        let detected = PeFile64::parse(bytes)
            .ok()
            .and_then(|pe| match pe.architecture() {
                object::Architecture::X86_64 => Some("amd64"),
                object::Architecture::Aarch64 => Some("arm64"),
                _ => None,
            });
        if let (Some(detected), Some(explicit)) = (detected, explicit) {
            ensure!(
                detected == explicit,
                "PMI architecture {detected} conflicts with --arch {explicit}"
            );
        }
        let Some(architecture) = detected.or(explicit) else {
            bail!("cannot determine PMI architecture; supply --arch");
        };
        Ok(Self {
            os: super::cache::TARGET_OS.into(),
            architecture: architecture.into(),
            variant: None,
        })
    }
    fn host_architecture() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        }
    }
}
