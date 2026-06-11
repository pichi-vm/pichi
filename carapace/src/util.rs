//! Crate-internal utility helpers shared across modules. Foundation —
//! depends on `error::CarapaceError` (for `decode_hex`) and nothing
//! else inside the crate.

use crate::CarapaceError;

/// Encode bytes as lowercase hex. Single source of truth — chain.rs
/// (cycle error formatting) and dm/table.rs (verity digest + salt
/// tokens) both consume this. `write!` into the pre-sized `String`
/// avoids the per-byte `format!` allocation that an earlier
/// `s.push_str(&format!("{b:02x}"))` formulation paid.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    write_hex_lower(&mut s, bytes).expect("write to String never fails");
    s
}

/// Write bytes as lowercase hex into any `fmt::Write`. Lets the dm-table
/// renderer land verity digest + salt directly in the kernel-ABI buffer
/// without an intermediate `String` per target.
pub(crate) fn write_hex_lower<W: std::fmt::Write>(w: &mut W, bytes: &[u8]) -> std::fmt::Result {
    for b in bytes {
        write!(w, "{b:02x}")?;
    }
    Ok(())
}

/// Decode lowercase or uppercase hex into bytes. Strips `0x` prefix +
/// surrounding whitespace. Used by both the CLI (parsing the operator's
/// `--root` argument) and the chain walker (`walk_from_hex` entry
/// point). Living in `util` rather than `cli` keeps the `chain →
/// decode_hex` import a downward dependency.
pub(crate) fn decode_hex(s: &str) -> Result<Vec<u8>, CarapaceError> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if s.len() % 2 != 0 {
        return Err(CarapaceError::Usage(format!(
            "hex string has odd length: {} chars",
            s.len()
        )));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16)
            .map_err(|e| CarapaceError::Usage(format!("invalid hex: {e}")))?;
        out.push(byte);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_pairs() {
        assert_eq!(hex_lower(&[0x00, 0x01, 0xAB, 0xCD, 0xFF]), "0001abcdff");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn decode_hex_strips_0x_prefix_and_whitespace() {
        assert_eq!(decode_hex("0xABCD").unwrap(), vec![0xab, 0xcd]);
        assert_eq!(decode_hex("  abcd\n").unwrap(), vec![0xab, 0xcd]);
        assert_eq!(decode_hex("ABCD").unwrap(), vec![0xab, 0xcd]);
    }

    #[test]
    fn decode_hex_rejects_odd_length() {
        assert!(matches!(decode_hex("abc"), Err(CarapaceError::Usage(_))));
    }

    #[test]
    fn decode_hex_rejects_non_hex_digits() {
        assert!(matches!(decode_hex("xyz!"), Err(CarapaceError::Usage(_))));
    }
}
