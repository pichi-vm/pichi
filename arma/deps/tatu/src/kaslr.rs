//! arm64 KASLR seed patching for the measured base DTB.
//!
//! arma plants an 8-byte zero `/chosen/kaslr-seed` placeholder in the measured
//! base DTB; tatu overwrites it with guest entropy before the overlay merge, so
//! a `CONFIG_RANDOMIZE_BASE` kernel reads a fresh, guest-controlled seed from the
//! merged DTB and randomizes its virtual base. The entropy source (guest `RNDR`)
//! lives in `arch_aarch64`; the byte-level patch lives here as a pure function so
//! it can be unit-tested on the host without a guest.

use devtree::{NodeView, Tree, TreeView};

/// Overwrite the 8-byte `/chosen/kaslr-seed` value in `blob` with `seed`
/// (big-endian, the FDT property byte order). Returns `false` and leaves `blob`
/// unchanged if the property is absent or not exactly 8 bytes.
///
/// Only the property's value bytes are rewritten — the DTB's structure block,
/// strings, and all offsets are untouched — so the blob stays valid for the
/// subsequent overlay merge. The parse borrow is confined to a sub-scope so the
/// in-place write needs no `unsafe` and cannot alias the parsed view.
pub fn patch_kaslr_seed_bytes(blob: &mut [u8], seed: u64) -> bool {
    let off = {
        let tree: Tree<'_> = match Tree::parse(blob) {
            Ok(t) => t,
            Err(_) => return false,
        };
        match tree
            .find_path("/chosen")
            .and_then(|c| c.property("kaslr-seed"))
        {
            // Byte offset of the value within the blob (same allocation).
            Some(p) if p.as_ref().len() == 8 => {
                p.as_ref().as_ptr() as usize - blob.as_ptr() as usize
            }
            _ => return false,
        }
    };
    blob[off..off + 8].copy_from_slice(&seed.to_be_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn dtc_compile(dts: &str) -> Option<Vec<u8>> {
        let mut child = Command::new("dtc")
            .args(["-I", "dts", "-O", "dtb"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(dts.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    /// A base DTB shaped like arma's: /chosen with a zero kaslr-seed placeholder.
    const BASE_WITH_SEED: &str = r#"/dts-v1/;
/ { #address-cells = <2>; #size-cells = <2>;
    chosen { bootargs = "console=ttyAMA0"; kaslr-seed = <0x0 0x0>; }; };"#;

    const BASE_NO_SEED: &str = r#"/dts-v1/;
/ { #address-cells = <2>; #size-cells = <2>;
    chosen { bootargs = "console=ttyAMA0"; }; };"#;

    fn read_seed(blob: &[u8]) -> u64 {
        let tree: Tree<'_> = Tree::parse(blob).unwrap();
        let p = tree
            .find_path("/chosen")
            .unwrap()
            .property("kaslr-seed")
            .unwrap();
        let b = p.as_ref();
        u64::from_be_bytes(b.try_into().unwrap())
    }

    #[test]
    fn patches_seed_in_place_and_stays_parseable() {
        let Some(mut blob) = dtc_compile(BASE_WITH_SEED) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        assert_eq!(read_seed(&blob), 0, "placeholder starts zero");
        let len_before = blob.len();
        let seed = 0x0123_4567_89AB_CDEF;
        assert!(patch_kaslr_seed_bytes(&mut blob, seed));
        // Same length (in-place value rewrite) and the tree still parses with the
        // new seed — proving the structure/offsets are intact for the merge.
        assert_eq!(blob.len(), len_before);
        assert_eq!(read_seed(&blob), seed, "seed updated to the patched value");
        // bootargs (a sibling property) is untouched.
        let tree: Tree<'_> = Tree::parse(&blob).unwrap();
        assert!(
            tree.find_path("/chosen")
                .unwrap()
                .property("bootargs")
                .is_some()
        );
    }

    #[test]
    fn no_seed_property_is_a_noop() {
        let Some(mut blob) = dtc_compile(BASE_NO_SEED) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let before = blob.clone();
        assert!(!patch_kaslr_seed_bytes(&mut blob, 0xDEAD_BEEF));
        assert_eq!(blob, before, "absent seed leaves the blob unchanged");
    }
}
