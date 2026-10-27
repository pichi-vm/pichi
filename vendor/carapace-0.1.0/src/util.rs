// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Crate-internal utility helpers: lowercase hex encoding.

/// Encode bytes as lowercase hex. Used by assemble.rs (dm device names)
/// and chain.rs (cycle error messages).
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    write_hex_lower(&mut s, bytes).expect("write to String never fails");
    s
}

/// Write bytes as lowercase hex into any `fmt::Write`.
pub(crate) fn write_hex_lower<W: std::fmt::Write>(w: &mut W, bytes: &[u8]) -> std::fmt::Result {
    for b in bytes {
        write!(w, "{b:02x}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_pairs() {
        assert_eq!(hex_lower(&[0x00, 0x01, 0xAB, 0xCD, 0xFF]), "0001abcdff");
        assert_eq!(hex_lower(&[]), "");
    }
}
