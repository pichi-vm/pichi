// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Lazily generates `tests/fixtures/sparse-1mib.raw` if missing.
//! Per RESEARCH §Open-Q #4: avoids committing a binary blob; keeps the
//! fixture's pattern visible in code so tests/CI both consume the same file.

use std::fs;
use std::path::Path;

fn main() {
    // 1 MiB image at chunk_size = 32 sectors = 16 KiB → 64 chunks.
    const CHUNK_BYTES: usize = 16 * 1024;
    const NUM_CHUNKS: usize = 64;

    println!("cargo:rerun-if-changed=build.rs");
    // Fixture lives at workspace root, NOT in this crate's directory.
    // CARGO_MANIFEST_DIR for this build script is .../tools/import/.
    // Ascend two levels to reach workspace root.
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root reachable from tools/import/");
    let fixture_dir = workspace_root.join("tests").join("fixtures");
    let fixture_path = fixture_dir.join("sparse-1mib.raw");

    if fixture_path.exists() {
        return;
    }

    fs::create_dir_all(&fixture_dir).expect("create tests/fixtures/");

    // Mark chunks 5, 17, 42, 60 as non-zero with distinct fill patterns
    // (RESEARCH §"Round-trip self-test" specifies "100 chunks, 5/17/42/99
    // marked"; we scale to 64 chunks fitting in 1 MiB).
    let mut buf = vec![0u8; CHUNK_BYTES * NUM_CHUNKS];
    for &(idx, fill) in &[(5usize, 0xA1u8), (17, 0xB2), (42, 0xC3), (60, 0xD4)] {
        let off = idx * CHUNK_BYTES;
        buf[off..off + CHUNK_BYTES].fill(fill);
    }
    fs::write(&fixture_path, &buf).expect("write fixture");
}
