#![no_main]

use core::num::NonZeroU32;

use devtree::{NodeView, PropertyView, Tree, TreeView};
use libfuzzer_sys::fuzz_target;

// Drive every public read path with arbitrary bytes. Contract: no
// panics, no infinite loops, no unbounded allocations. After eager
// parse, walks are infallible.
fuzz_target!(|data: &[u8]| {
    let fdt: Tree = match Tree::parse(data) {
        Ok(f) => f,
        Err(_) => return,
    };

    for entry in fdt.reservations() {
        let _ = entry;
    }

    // Default Tree's DEPTH cap is 64; walk_to that bound.
    walk(fdt.root(), 64);

    // find_phandle exercises depth-bounded recursion.
    let _ = fdt.find_phandle(NonZeroU32::new(1).unwrap());

    // AsRef returns the header-bounded slice; reparse must succeed.
    let _: Tree = Tree::parse(fdt.as_ref()).expect("as_ref roundtrip");
});

fn walk<N: NodeView>(node: N, depth: u32) {
    if depth == 0 {
        return;
    }
    for p in node.properties() {
        let _ = p.name();
        let _: &[u8] = p.as_ref();
    }
    for c in node.children() {
        walk(c, depth - 1);
    }
}
