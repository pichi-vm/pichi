//! Target-neutral PMI/development-tree launch preflight.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use dillo_devtree::platform::{Arch, Machine as PlatformMachine, SurveyError};
use dillo_machine::HostArchitecture;
use thiserror::Error;

use crate::placement::{self, MemoryPlan};
use crate::pmi_parse::{Action as PmiAction, FillKind, HostArch, ParseOptions};

/// Target-neutral launch facts derived before backend construction.
#[derive(Debug)]
pub struct LaunchPlan {
    pub bytes: Vec<u8>,
    pub parsed: crate::pmi_parse::ParsedPmi,
    pub merged_dtb: Vec<u8>,
    pub platform: PlatformMachine,
    pub memory: MemoryPlan,
    pub guest_writes: Vec<GuestWrite>,
}

/// One launch-time write into guest RAM.
#[derive(Debug)]
pub struct GuestWrite {
    pub section: String,
    pub gpa: u64,
    pub data: Vec<u8>,
}

impl LaunchPlan {
    /// Read a PMI file, validate target-neutral launch facts, and compute RAM
    /// placement from the DTB-declared platform.
    pub fn read(
        pmi_path: &Path,
        host_arch: HostArchitecture,
        platform: impl FnOnce(&[u8]) -> Result<PlatformMachine, SurveyError>,
        memory_mib: u32,
        vcpus: u32,
    ) -> Result<Self, LaunchError> {
        let mut bytes = Vec::new();
        File::open(pmi_path)
            .map_err(|source| LaunchError::ReadPmi {
                path: pmi_path.display().to_string(),
                source,
            })?
            .read_to_end(&mut bytes)
            .map_err(|source| LaunchError::ReadPmi {
                path: pmi_path.display().to_string(),
                source,
            })?;

        let pmi_arch = pmi_arch(host_arch);
        let parsed = crate::pmi_parse::parse(
            &bytes,
            &ParseOptions {
                host_arch: pmi_arch,
                memory_mib,
            },
        )?;
        validate_cpu_profile(parsed.cpu_profile.as_str(), pmi_arch)?;

        let dtb = merged_dtb(&bytes, &parsed)?.to_vec();
        let platform = platform(&dtb).map_err(LaunchError::Coverage)?;

        let load_ranges: Vec<(String, u64, u64)> = parsed
            .sections
            .iter()
            .map(|(name, section)| (name.clone(), section.gpa, section.virtual_size))
            .collect();
        platform
            .plan
            .cross_validate_loads(&load_ranges)
            .map_err(LaunchError::DtbCrossValidate)?;

        let must_cover: Vec<(u64, u64)> = parsed
            .sections
            .values()
            .map(|section| (section.gpa, section.virtual_size))
            .collect();
        let memory =
            placement::plan_around_regions(&must_cover, memory_mib, platform.placement_regions())?;
        let guest_writes = guest_writes(&bytes, &parsed, &memory, platform.arch, vcpus)?;

        Ok(Self {
            bytes,
            parsed,
            merged_dtb: dtb,
            platform,
            memory,
            guest_writes,
        })
    }
}

fn pmi_arch(host_arch: HostArchitecture) -> HostArch {
    match host_arch {
        HostArchitecture::X86_64 => HostArch::X86_64,
        HostArchitecture::Aarch64 => HostArch::Aarch64,
    }
}

/// Error produced by target-neutral launch preflight.
#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("read PMI {path}: {source}")]
    ReadPmi {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("PMI parse: {0}")]
    PmiParse(#[from] crate::pmi_parse::Error),

    #[error("merged_dtb section missing from parsed.sections")]
    MissingMergedDtb,

    #[error("base DTB coverage: {0}")]
    Coverage(SurveyError),

    #[error("base DTB / PE cross-validation: {0}")]
    DtbCrossValidate(SurveyError),

    #[error("merged_dtb section lies outside the PMI file")]
    MalformedMergedDtb,

    #[error("memory placement: {0}")]
    Placement(#[from] placement::PlanError),

    #[error("unrecognized cpu:profile {0:?} for {1:?}")]
    UnknownCpuProfile(String, HostArch),

    #[error("load section `{0}` lies outside the PMI file")]
    MalformedLoadSection(String),

    #[error("synthesize host DTBO: {0}")]
    DtboSynth(#[source] anyhow::Error),
}

impl LaunchError {
    /// Map to the documented dillo launch exit code categories.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ReadPmi { .. } | Self::PmiParse(_) => 10,
            Self::MissingMergedDtb
            | Self::Coverage(_)
            | Self::DtbCrossValidate(_)
            | Self::MalformedMergedDtb
            | Self::MalformedLoadSection(_)
            | Self::DtboSynth(_) => 11,
            Self::Placement(_) => 13,
            Self::UnknownCpuProfile(_, _) => 12,
        }
    }
}

fn guest_writes(
    bytes: &[u8],
    parsed: &crate::pmi_parse::ParsedPmi,
    memory: &MemoryPlan,
    arch: Arch,
    vcpus: u32,
) -> Result<Vec<GuestWrite>, LaunchError> {
    let mut writes = Vec::new();
    for action in &parsed.actions {
        match action {
            PmiAction::Load { section } => {
                let s = &parsed.sections[section];
                if s.file_size == 0 {
                    continue;
                }
                let data = read_section(bytes, s.file_offset, s.file_size)
                    .ok_or_else(|| LaunchError::MalformedLoadSection(section.clone()))?
                    .to_vec();
                writes.push(GuestWrite {
                    section: section.clone(),
                    gpa: s.gpa,
                    data,
                });
            }
            PmiAction::Fill {
                section,
                kind: FillKind::MergedDtbo,
            } => {
                let s = &parsed.sections[section];
                let data = crate::overlay::synthesize_dtbo(
                    &memory.memory_nodes,
                    vcpus,
                    arch.cpu_enable_method(),
                    crate::cpu_id::host_cpu_compatible(arch),
                    s.virtual_size,
                )
                .map_err(LaunchError::DtboSynth)?;
                writes.push(GuestWrite {
                    section: section.clone(),
                    gpa: s.gpa,
                    data,
                });
            }
        }
    }
    Ok(writes)
}

fn merged_dtb<'a>(
    bytes: &'a [u8],
    parsed: &crate::pmi_parse::ParsedPmi,
) -> Result<&'a [u8], LaunchError> {
    let dtb_info = parsed
        .sections
        .get(&parsed.merged_dtb_section)
        .ok_or(LaunchError::MissingMergedDtb)?;
    read_section(bytes, dtb_info.file_offset, dtb_info.file_size)
        .ok_or(LaunchError::MalformedMergedDtb)
}

fn read_section(bytes: &[u8], offset: u64, size: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let size = usize::try_from(size).ok()?;
    let end = start.checked_add(size)?;
    bytes.get(start..end)
}

/// Validate the `cpu:profile` name against the PMI machine architecture.
pub fn validate_cpu_profile(profile: &str, arch: HostArch) -> Result<(), LaunchError> {
    let recognized = match arch {
        HostArch::Aarch64 => parse_armv_profile(profile).is_some(),
        HostArch::X86_64 => matches!(
            profile,
            "x86-64-v1" | "x86-64-v2" | "x86-64-v3" | "x86-64-v4"
        ),
    };
    if recognized {
        Ok(())
    } else {
        Err(LaunchError::UnknownCpuProfile(profile.to_string(), arch))
    }
}

fn parse_armv_profile(s: &str) -> Option<(u32, u32)> {
    let body = s.strip_prefix("armv")?.strip_suffix("-a")?;
    let (major, minor) = body.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_profile_validation_accepts_known_x86_levels() {
        for profile in ["x86-64-v1", "x86-64-v2", "x86-64-v3", "x86-64-v4"] {
            validate_cpu_profile(profile, HostArch::X86_64).expect(profile);
        }
    }

    #[test]
    fn cpu_profile_validation_accepts_armv_profiles() {
        validate_cpu_profile("armv8.0-a", HostArch::Aarch64).expect("armv8.0-a");
        validate_cpu_profile("armv9.2-a", HostArch::Aarch64).expect("armv9.2-a");
    }

    #[test]
    fn cpu_profile_validation_rejects_arch_mismatch() {
        assert!(validate_cpu_profile("armv8.0-a", HostArch::X86_64).is_err());
        assert!(validate_cpu_profile("x86-64-v2", HostArch::Aarch64).is_err());
    }

    #[test]
    fn read_section_checks_bounds() {
        assert_eq!(read_section(b"abcdef", 1, 3), Some(&b"bcd"[..]));
        assert!(read_section(b"abcdef", 4, 3).is_none());
        assert!(read_section(b"abcdef", u64::MAX, 1).is_none());
    }
}
