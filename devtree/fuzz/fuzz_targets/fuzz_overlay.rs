#![no_main]

use devtree::{Error, Overlay, OverlayView, Tree};
use libfuzzer_sys::fuzz_target;

// Apply an arbitrary byte string as an overlay against a fixed, valid
// base. Stresses fragment resolution, fixup parsing, phandle-shift
// arithmetic, and the merged-output emission path. Must never panic.
fuzz_target!(|data: &[u8]| {
    const BASE: &[u8] = include_bytes!("../../tests/data/overlay_base.dtb");
    let base: Tree = match Tree::parse(BASE) {
        Ok(b) => b,
        Err(_) => return,
    };
    let Ok(overlay) = Overlay::<16, 64, 4>::parse(data) else {
        return;
    };

    // Pre-merge introspection: each fragment must have a target and a
    // walkable __overlay__ node.
    for frag in overlay.fragments() {
        let _ = frag.target();
        let _ = frag.node();
    }

    // Probe for needed size via empty-buffer apply, then merge.
    // `apply` runs structural overflow checks before the buffer-size
    // check, so a crafted overlay can legitimately return SizeOverflow
    // (or other errors) from the probe — fall through silently.
    // `Ok(_)` with an empty destination violates the apply contract
    // (total_upper >= FDT_HEADER_SIZE > 0, so dst.len() == 0 must
    // return BufferTooSmall) and must be surfaced as a panic.
    let needed = match overlay.apply(&base, &mut []) {
        Err(Error::BufferTooSmall { needed }) => needed,
        Ok(_) => panic!("apply succeeded with an empty destination buffer"),
        Err(_) => return,
    };
    let mut buf = vec![0u8; needed];
    if let Ok(written) = overlay.apply(&base, &mut buf) {
        // The merged emission MUST parse as a valid base FDT.
        buf.truncate(written);
        let reparsed: Tree = Tree::parse(&buf).expect("reparse of merged output");
        // And re-emitting the reparsed tree must be byte-identical.
        let reparsed_bytes: &[u8] = reparsed.as_ref();
        assert_eq!(reparsed_bytes, &buf[..written]);
    }
});
