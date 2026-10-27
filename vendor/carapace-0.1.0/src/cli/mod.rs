// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CLI entry point. Single verb: `mount`. Hand-rolled parser — no clap.

use std::io;
use std::process::ExitCode;

#[cfg(feature = "gpt")]
use std::path::PathBuf;

#[cfg(feature = "gpt")]
use carapace::{Assemble, Chain, DevId, FromGpt};

/// Decode hex string (with optional 0x prefix, whitespace trimmed) to bytes.
#[cfg(feature = "gpt")]
fn decode_hex(s: &str) -> io::Result<Vec<u8>> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("hex string has odd length: {} chars", s.len()),
        ));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid hex: {e}"))
        })?;
        out.push(byte);
    }
    Ok(out)
}

const HELP: &str = "\
carapace — assemble carapace block-device chains (read-side).

USAGE:
    carapace mount <root-hex> <dir> [--type <fs>]
    carapace --help
    carapace --version

ARGS:
    <root-hex>   Trusted chain root, hex-encoded (>= 64 hex chars for sha256).
    <dir>        Mount point directory.

OPTIONS:
    --type <fs>  Filesystem type passed to mount(8). Optional; auto-detected
                 if omitted.

`carapace mount` walks the chain backward from <root-hex> via PARTUUID
lookup in /sys/class/block/*/uevent, validates each scute against the RDP
whitelist, builds the dm-verity + dm-snapshot stack, then mounts the top
device read-only at <dir>. On success the dm stack is anchored by the
mount; `umount <dir>` tears it down via deferred-remove.
";

pub(crate) fn run() -> ExitCode {
    match parse_and_dispatch(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("carapace: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "gpt")]
#[derive(Debug)]
struct MountArgs {
    root_hex: String,
    dir: PathBuf,
    fstype: Option<String>,
}

fn parse_and_dispatch(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let Some(verb) = args.next() else {
        eprint!("{HELP}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing verb"));
    };
    match verb.as_str() {
        "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        "-V" | "--version" => {
            println!("carapace {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "mount" => {
            #[cfg(feature = "gpt")]
            {
                let mount_args = parse_mount(args)?;
                do_mount(mount_args)
            }
            #[cfg(not(feature = "gpt"))]
            {
                let _ = args;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the `mount` verb requires the `gpt` feature (enabled by default)",
                ))
            }
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown verb {other:?} (expected `mount`, `--help`, or `--version`)"),
        )),
    }
}

#[cfg(feature = "gpt")]
fn parse_mount(mut args: impl Iterator<Item = String>) -> io::Result<MountArgs> {
    let root_hex = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mount: missing <root-hex>"))?;
    let dir = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mount: missing <dir>"))?;
    let mut fstype: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--type" | "-t" => {
                let val = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{arg} requires a value"),
                    )
                })?;
                fstype = Some(val);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("mount: unexpected argument {other:?}"),
                ));
            }
        }
    }
    Ok(MountArgs {
        root_hex,
        dir: PathBuf::from(dir),
        fstype,
    })
}

#[cfg(feature = "gpt")]
fn do_mount(args: MountArgs) -> io::Result<()> {
    let root = decode_hex(&args.root_hex)?;
    let chain = Chain::from_gpt(&root)?;
    let device = chain.assemble()?;
    let minor = DevId::try_from(device.metadata()?)?.minor();
    let dev_path = format!("/dev/dm-{minor}");

    let mut cmd = std::process::Command::new("mount");
    cmd.arg("-o").arg("ro");
    if let Some(ref t) = args.fstype {
        cmd.arg("-t").arg(t);
    }
    cmd.arg(&dev_path).arg(&args.dir);

    let status = cmd.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "mount failed with exit code: {status}"
        )));
    }

    // The mount now retains the already deferred-removal stack.
    drop(device);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> std::vec::IntoIter<String> {
        items
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_requires_root_hex() {
        let e = parse_mount(args(&[])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_requires_dir() {
        let e = parse_mount(args(&["deadbeef"])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_accepts_type() {
        let a = parse_mount(args(&["deadbeef", "/mnt", "--type", "ext4"])).unwrap();
        assert_eq!(a.root_hex, "deadbeef");
        assert_eq!(a.dir, PathBuf::from("/mnt"));
        assert_eq!(a.fstype, Some("ext4".into()));
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_accepts_short_type_flag() {
        let a = parse_mount(args(&["deadbeef", "/mnt", "-t", "squashfs"])).unwrap();
        assert_eq!(a.fstype, Some("squashfs".into()));
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_no_fstype() {
        let a = parse_mount(args(&["deadbeef", "/mnt"])).unwrap();
        assert_eq!(a.fstype, None);
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_rejects_unknown_flag() {
        let e = parse_mount(args(&["deadbeef", "/mnt", "--bogus"])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn parse_mount_rejects_type_missing_value() {
        let e = parse_mount(args(&["deadbeef", "/mnt", "--type"])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn unknown_verb_returns_invalid_input() {
        let e = parse_and_dispatch(args(&["detach"])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }
}
