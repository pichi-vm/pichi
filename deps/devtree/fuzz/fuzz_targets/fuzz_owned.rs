#![no_main]

use devtree::{OwnedNode, OwnedTree, Tree};
use libfuzzer_sys::fuzz_target;

// Materialize an owned tree from an arbitrary (but successfully parsed) blob,
// read every node/property through the owned API, round-trip it through the
// serializer, then drain the whole tree by move. Exercises `materialize`, the
// inherent read accessors, the value parsers, `encode`, and
// `remove_child`/`remove_property`. Must never panic; a full drain must leave
// the root empty; and a materialized tree must encode and re-parse.
fuzz_target!(|data: &[u8]| {
    let Ok(tree): Result<Tree, _> = Tree::parse(data) else {
        return;
    };
    let mut owned = OwnedTree::materialize(&tree);
    walk(owned.root());
    // Serialize round-trip: a materialized tree must re-encode and re-parse.
    if let Ok(bytes) = owned.encode() {
        let reparsed: Result<Tree, _> = Tree::parse(&bytes);
        assert!(reparsed.is_ok(), "encoded tree failed to re-parse");
    }
    drain(owned.root_mut());
    assert_eq!(owned.root().properties().count(), 0);
    assert_eq!(owned.root().children().count(), 0);
});

fn walk(node: &OwnedNode) {
    let _ = node.name();
    let _ = node.path();
    let _ = node.phandle();
    let _ = node.address_cells();
    let _ = node.size_cells();
    for p in node.properties() {
        let _ = p.as_u32();
        let _ = p.as_u64();
        let _ = p.as_str();
        let _ = p.as_u32s().map(Iterator::count);
        let _ = p.as_strs().map(Iterator::count);
    }
    for c in node.children() {
        walk(c);
    }
}

fn drain(node: &mut OwnedNode) {
    while let Some(mut child) = node.remove_child_at(0) {
        drain(&mut child);
    }
    let names: Vec<String> = node.properties().map(|p| p.name().to_string()).collect();
    for n in names {
        let _ = node.remove_property(&n);
    }
}
