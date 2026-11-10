// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, ensure};
use pichi_launch::Input;
use std::process::Command;

fn command(input: &Input) -> Command {
    let mut command = Command::new("/usr/bin/conglobate");
    command.args([
        "build",
        "/run/pichi/repo",
        "--uid-map=100000:65536",
        "--gid-map=100000:65536",
        "--cache=/run/pichi/cache",
        "--output=/run/pichi/output",
    ]);
    command.arg(format!("--ref={}", input.r#ref));
    command.arg(format!("--plan={}", input.plan));
    command.arg(format!("--lock={}", input.lock));
    command.arg(format!("--context={}", input.context));
    if let Some(location) = &input.location {
        command.arg(format!("--location={location}"));
    }
    command
}

pub(crate) fn run(input: &Input) -> anyhow::Result<()> {
    let status = command(input).status().context("start Conglobate")?;
    ensure!(status.success(), "Conglobate failed: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_arguments_preserve_literal_input_and_optional_location() {
        for location in [None, Some("https://example.org/repo".into())] {
            let input = Input {
                r#ref: "-not-an-option; $(false)".into(),
                plan: "nested plan.yaml".into(),
                lock: "lock.yaml".into(),
                context: ".".into(),
                location,
            };
            let command = command(&input);
            assert_eq!(command.get_program(), "/usr/bin/conglobate");
            let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(&args[..2], ["build", "/run/pichi/repo"]);
            for fixed in [
                "--uid-map=100000:65536",
                "--gid-map=100000:65536",
                "--cache=/run/pichi/cache",
                "--output=/run/pichi/output",
                "--ref=-not-an-option; $(false)",
                "--plan=nested plan.yaml",
                "--lock=lock.yaml",
                "--context=.",
            ] {
                assert!(args.contains(&fixed));
            }
            assert_eq!(
                args.contains(&"--location=https://example.org/repo"),
                input.location.is_some()
            );
        }
    }
}
