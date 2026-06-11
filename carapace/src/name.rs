//! dm device-name validation, shared by `attach`/`detach` and the CLI parser.

use crate::CarapaceError;

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
pub fn validate_dm_name(name: &str) -> Result<(), CarapaceError> {
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
