// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use pichi_launch::Input;
use std::process::Command;

/// A host request forwarded to the engine for validation and execution.
pub(crate) struct Build(pub Input);

impl Build {
    pub(crate) fn command(self) -> Command {
        let mut command = Command::new("/usr/bin/conglobate");
        command.args([
            "build",
            "/run/pichi/repo",
            "--cache",
            "/run/pichi/cache",
            "--output",
            "/run/pichi/output",
            "--uid-map",
            "100000:65536",
            "--gid-map",
            "100000:65536",
        ]);
        // Equals keeps leading hyphens inside values, never as CLI options.
        command.args([
            format!("--ref={}", self.0.r#ref),
            format!("--context={}", self.0.context),
            format!("--plan={}", self.0.plan),
            format!("--lock={}", self.0.lock),
        ]);
        if let Some(location) = self.0.location {
            command.arg(format!("--location={location}"));
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_literal_inputs_and_fixed_environment() {
        let command = Build(Input {
            r#ref: "refs/heads/main".into(),
            context: "project".into(),
            plan: "-plan $(touch bad).yaml".into(),
            lock: "locks/selected.lock".into(),
            location: Some("https://example.test/a?name='value'".into()),
        })
        .command();
        assert_eq!(command.get_program(), "/usr/bin/conglobate");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "build",
                "/run/pichi/repo",
                "--cache",
                "/run/pichi/cache",
                "--output",
                "/run/pichi/output",
                "--uid-map",
                "100000:65536",
                "--gid-map",
                "100000:65536",
                "--ref=refs/heads/main",
                "--context=project",
                "--plan=-plan $(touch bad).yaml",
                "--lock=locks/selected.lock",
                "--location=https://example.test/a?name='value'",
            ]
        );
    }

    #[test]
    fn absent_location_stays_absent() {
        let command = Build(Input {
            r#ref: "HEAD".into(),
            context: ".".into(),
            plan: "pichi.yaml".into(),
            lock: "pichi.lock".into(),
            location: None,
        })
        .command();
        assert!(
            !command
                .get_args()
                .any(|arg| arg.to_str().unwrap().starts_with("--location="))
        );
    }
}
