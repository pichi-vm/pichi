//! arm64 KASLR seed: a trusted overlay tatu merges onto the measured base DTB.
//!
//! `kaslr-seed` is guest-generated entropy, so it MUST NOT live in the measured
//! base DTB (it would make the launch measurement non-deterministic). Instead
//! tatu carries a tiny pre-serialized overlay — [`KASLR_OVERLAY_TEMPLATE`],
//! compiled from `kaslr_overlay.dts` with `dtc` and committed alongside it —
//! whose `/chosen/kaslr-seed` is a zero placeholder. At boot tatu patches the
//! placeholder with guest entropy ([`patch_overlay_seed`]) and merges the
//! overlay onto the base *before* the host overlay, so a `CONFIG_RANDOMIZE_BASE`
//! kernel reads a fresh, guest-controlled seed from the final merged DTB.
//!
//! The entropy source (guest `RNDR`) and the two-merge orchestration live in
//! `arch_aarch64`; the byte-level patch lives here as a pure function so it can
//! be unit-tested on the host without a guest.

use devtree::{NodeView, Tree, TreeView};

/// Pre-serialized kaslr-seed overlay (see `kaslr_overlay.dts`). Adds
/// `/chosen/kaslr-seed` via a `target-path` fragment; the seed is a zero
/// placeholder patched at boot. Regenerate after editing the `.dts`:
/// `dtc -I dts -O dtb -o kaslr_overlay.dtbo kaslr_overlay.dts`.
pub const KASLR_OVERLAY_TEMPLATE: &[u8] = include_bytes!("kaslr_overlay.dtbo");

/// Path to the seed-bearing node inside the overlay template.
const OVERLAY_SEED_NODE: &str = "/fragment@0/__overlay__";

/// Overwrite the 8-byte `kaslr-seed` value in the overlay `blob` with `seed`
/// (big-endian, the FDT property byte order). Returns `false` and leaves `blob`
/// unchanged if the property is absent or not exactly 8 bytes.
///
/// Only the property's value bytes are rewritten — the DTB's structure block,
/// strings, and all offsets are untouched — so the blob stays a valid overlay
/// for the subsequent merge. The parse borrow is confined to a sub-scope so the
/// in-place write needs no `unsafe` and cannot alias the parsed view.
pub fn patch_overlay_seed(blob: &mut [u8], seed: u64) -> bool {
    let off = {
        let tree: Tree<'_> = match Tree::parse(blob) {
            Ok(t) => t,
            Err(_) => return false,
        };
        match tree
            .find_path(OVERLAY_SEED_NODE)
            .and_then(|n| n.property("kaslr-seed"))
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
    use devtree::{Overlay, OverlayView, PropertyView};
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

    /// A base DTB shaped like arma's post-redesign output: /chosen with bootargs
    /// and NO kaslr-seed (the seed now comes from the overlay).
    const BASE_NO_SEED: &str = r#"/dts-v1/;
/ { #address-cells = <2>; #size-cells = <2>;
    chosen { bootargs = "console=ttyAMA0"; }; };"#;

    fn read_overlay_seed(blob: &[u8]) -> u64 {
        let tree: Tree<'_> = Tree::parse(blob).unwrap();
        let p = tree
            .find_path(OVERLAY_SEED_NODE)
            .unwrap()
            .property("kaslr-seed")
            .unwrap();
        u64::from_be_bytes(p.as_ref().try_into().unwrap())
    }

    #[test]
    fn template_placeholder_starts_zero() {
        assert_eq!(read_overlay_seed(KASLR_OVERLAY_TEMPLATE), 0);
    }

    #[test]
    fn patches_overlay_seed_in_place_and_stays_parseable() {
        let mut blob = KASLR_OVERLAY_TEMPLATE.to_vec();
        let len_before = blob.len();
        let seed = 0x0123_4567_89AB_CDEF;
        assert!(patch_overlay_seed(&mut blob, seed));
        // Same length (in-place value rewrite); still a valid overlay.
        assert_eq!(blob.len(), len_before);
        assert_eq!(read_overlay_seed(&blob), seed);
        let reparsed: Result<Overlay<'_>, _> = Overlay::parse(&blob);
        assert!(reparsed.is_ok(), "patched blob still parses");
    }

    #[test]
    fn no_seed_property_is_a_noop() {
        // A degenerate overlay with no kaslr-seed: patch is a no-op.
        let Some(mut blob) = dtc_compile(
            r#"/dts-v1/;
/ { fragment@0 { target-path = "/chosen"; __overlay__ { }; }; };"#,
        ) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let before = blob.clone();
        assert!(!patch_overlay_seed(&mut blob, 0xDEAD_BEEF));
        assert_eq!(blob, before, "absent seed leaves the blob unchanged");
    }

    /// End-to-end of the first merge: patch the committed template, merge it onto
    /// a seed-free base, and confirm the result carries the patched
    /// `/chosen/kaslr-seed` while the base's own /chosen content survives.
    #[test]
    fn merges_patched_seed_onto_base() {
        let Some(base_blob) = dtc_compile(BASE_NO_SEED) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let base: Tree<'_> = Tree::parse(&base_blob).unwrap();

        let mut tpl = KASLR_OVERLAY_TEMPLATE.to_vec();
        let seed = 0xCAFE_F00D_1234_5678;
        assert!(patch_overlay_seed(&mut tpl, seed));
        let overlay: Overlay<'_> = Overlay::parse(&tpl).unwrap();

        let mut out = [0u8; 16 * 1024];
        let n = overlay.apply(&base, &mut out).expect("merge ok");
        let merged: Tree<'_> = Tree::parse(&out[..n]).unwrap();

        let chosen = merged.find_path("/chosen").expect("/chosen present");
        let p = chosen.property("kaslr-seed").expect("seed merged in");
        assert_eq!(u64::from_be_bytes(p.as_ref().try_into().unwrap()), seed);
        // Base content preserved through the merge.
        assert_eq!(
            chosen.property("bootargs").unwrap().as_str().unwrap(),
            "console=ttyAMA0"
        );
    }
}
