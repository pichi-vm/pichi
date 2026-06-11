//! CLI entry point. Two verbs: `attach` and `detach`. All-named
//! arguments, no positionals (matches v1 spec). Hand-rolled parser —
//! lane 04 found the `clap` derive feature pulled 16 transitive crates
//! and ~38% of the release `.text` for two verbs and three flags;
//! that's the textbook case for not earning its keep.

use crate::CarapaceError;
use std::process::ExitCode;

mod attach;
mod detach;

const HELP: &str = "\
carapace — assemble carapace block-device chains. Read-only at the operator surface.

USAGE:
    carapace attach --name <NAME> --root <HEX>
    carapace detach --name <NAME>
    carapace --help
    carapace --version

FLAGS:
    -n, --name <NAME>    Operator-visible /dev/mapper/<NAME>
    -r, --root <HEX>     Trusted chain root, lowercase hex (\u{2265} 64 chars)

attach walks the chain backward from --root, validates parameters
against the RDP whitelist, builds the dm stack, and prints the
operator-visible /dev/dm-<minor> path on success. Partitions are
discovered by PARTUUID lookup against /sys/class/block/*/uevent —
every visible GPT-partscanned block device contributes; no --storage
flag and no GPT parser.

detach is best-effort removal of every dm device prefixed <NAME>.
";

pub(crate) fn run() -> ExitCode {
    let result = parse_and_dispatch(std::env::args().skip(1));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("carapace: {e}");
            // 2 for chain-rejection (initrd can fail-closed cleanly);
            // 1 for operational failure (kernel ioctl, I/O, CLI usage).
            if e.is_adversary_rejection() {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn parse_and_dispatch(mut args: impl Iterator<Item = String>) -> Result<(), CarapaceError> {
    let Some(verb) = args.next() else {
        eprint!("{HELP}");
        return Err(CarapaceError::Usage("missing verb".into()));
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
        "attach" => {
            let (name, root) = parse_attach(args)?;
            attach::run(&name, &root)
        }
        "detach" => {
            let name = parse_detach(args)?;
            detach::run(&name)
        }
        other => Err(CarapaceError::Usage(format!(
            "unknown verb {other:?} (expected `attach`, `detach`, `--help`, or `--version`)"
        ))),
    }
}

fn parse_attach(args: impl Iterator<Item = String>) -> Result<(String, String), CarapaceError> {
    let mut name: Option<String> = None;
    let mut root: Option<String> = None;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--name" => name = Some(value_for(&arg, iter.next())?),
            "-r" | "--root" => root = Some(value_for(&arg, iter.next())?),
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => {
                return Err(CarapaceError::Usage(format!(
                    "attach: unexpected argument {other:?}"
                )));
            }
        }
    }
    let name = name.ok_or_else(|| CarapaceError::Usage("attach: --name is required".into()))?;
    validate_dm_name(&name)?;
    let root = root.ok_or_else(|| CarapaceError::Usage("attach: --root is required".into()))?;
    Ok((name, root))
}

fn parse_detach(args: impl Iterator<Item = String>) -> Result<String, CarapaceError> {
    let mut name: Option<String> = None;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--name" => name = Some(value_for(&arg, iter.next())?),
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => {
                return Err(CarapaceError::Usage(format!(
                    "detach: unexpected argument {other:?}"
                )));
            }
        }
    }
    let name = name.ok_or_else(|| CarapaceError::Usage("detach: --name is required".into()))?;
    validate_dm_name(&name)?;
    Ok(name)
}

fn value_for(flag: &str, value: Option<String>) -> Result<String, CarapaceError> {
    value.ok_or_else(|| CarapaceError::Usage(format!("{flag} requires a value")))
}

/// Reject `--name` values that would smuggle path-traversal, terminal
/// escapes, dm-table-line tokens, or `/dev/mapper/<…>` lookalikes
/// into downstream `format!("{name}-…")` and `Path::exists` paths.
///
/// The kernel's dm name allowlist is roughly C-identifier-like up to
/// 127 bytes. We're tighter: alphanumeric + `_` + `-` + `.`, length
/// 1..=120 (leaving 7 chars headroom for our `-z0` / `-v<NN>` /
/// `-s<NN>` suffixes which max out at 5 chars at MAX_CHAIN_DEPTH=32).
///
/// Specifically rejected: empty, `/`, `..`, `\`, whitespace, control
/// bytes (incl. ESC for terminal escapes), `%` (printf formats in
/// downstream tools), shell metacharacters.
pub(crate) fn validate_dm_name(name: &str) -> Result<(), CarapaceError> {
    if name.is_empty() {
        return Err(CarapaceError::Usage("--name must be non-empty".into()));
    }
    if name.len() > 120 {
        return Err(CarapaceError::Usage(format!(
            "--name too long: {} bytes (max 120)",
            name.len()
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.'))
    {
        return Err(CarapaceError::Usage(format!(
            "--name contains illegal character {bad:?}; allowed: ASCII alphanumeric, `_`, `-`, `.`"
        )));
    }
    if name == "." || name == ".." {
        return Err(CarapaceError::Usage(format!(
            "--name {name:?} is reserved (path-traversal hazard)"
        )));
    }
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

    #[test]
    fn parse_attach_long_form() {
        let (n, r) = parse_attach(args(&["--name", "root", "--root", "deadbeef"])).unwrap();
        assert_eq!(n, "root");
        assert_eq!(r, "deadbeef");
    }

    #[test]
    fn parse_attach_short_form() {
        let (n, r) = parse_attach(args(&["-n", "root", "-r", "deadbeef"])).unwrap();
        assert_eq!(n, "root");
        assert_eq!(r, "deadbeef");
    }

    #[test]
    fn parse_attach_flag_order_is_arbitrary() {
        let (n, r) = parse_attach(args(&["--root", "deadbeef", "--name", "root"])).unwrap();
        assert_eq!(n, "root");
        assert_eq!(r, "deadbeef");
    }

    #[test]
    fn parse_attach_rejects_missing_required() {
        assert!(matches!(
            parse_attach(args(&["--name", "root"])),
            Err(CarapaceError::Usage(_))
        ));
        assert!(matches!(
            parse_attach(args(&["--root", "deadbeef"])),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn parse_attach_rejects_unknown_flag() {
        assert!(matches!(
            parse_attach(args(&["--name", "x", "--root", "y", "--bogus"])),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn parse_attach_rejects_value_missing_for_flag() {
        assert!(matches!(
            parse_attach(args(&["--name"])),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn parse_detach_requires_name() {
        assert_eq!(parse_detach(args(&["--name", "root"])).unwrap(), "root");
        assert!(matches!(
            parse_detach(args(&[])),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn validate_dm_name_accepts_safe_inputs() {
        for ok in ["root", "carapace-test-1234", "a", "fs.root.0", "X_Y_Z"] {
            assert!(validate_dm_name(ok).is_ok(), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn validate_dm_name_rejects_unsafe_inputs() {
        let bad = [
            "",                 // empty
            "..",               // path-traversal sentinel
            ".",                // ditto
            "../control",       // path traversal — would alias /dev/mapper/control
            "/etc/shadow",      // absolute path
            "name with spaces", // whitespace
            "tab\there",        // control char
            "esc\x1b[31m",      // ANSI escape
            "name\0nul",        // NUL
            "name%s",           // printf format
            "name;rm -rf /",    // shell metachar
            "name\nnewline",    // newline
        ];
        for bad in bad {
            assert!(
                matches!(validate_dm_name(bad), Err(CarapaceError::Usage(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_dm_name_rejects_overlong() {
        let long = "a".repeat(121);
        assert!(matches!(
            validate_dm_name(&long),
            Err(CarapaceError::Usage(_))
        ));
        // Boundary: exactly 120 is allowed.
        let max = "a".repeat(120);
        assert!(validate_dm_name(&max).is_ok());
    }
}
