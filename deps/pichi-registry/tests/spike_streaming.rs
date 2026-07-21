// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "http-client")]
#![allow(missing_docs)] // SPIKE tests; not part of public API
//
// Phase 44 Plan 01 Task 3 — Wave-0 SPIKE tests for Assumptions A2/A3/A5
// (RESEARCH §"Assumptions Log" lines 1626-1638). Failures here block
// Plan 03's HttpRegistry design and force a documented redesign.

use std::io::Write;
use tokio_util::io::SyncIoBridge;

/// SPIKE A3: `SyncIoBridge::new` MUST work under
/// `Builder::new_current_thread()` when `enable_all()` is set (which
/// configures the blocking pool).
///
/// Note on shape: per tokio-util docs, `SyncIoBridge::new(async_writer)`
/// produces a SYNC `std::io::Write` that bridges to the async writer via
/// `Handle::block_on`. It MUST be moved into a `spawn_blocking` task —
/// calling its sync methods directly inside `block_on` would re-enter the
/// runtime and panic.
///
/// If this test PANICS or fails, Plan 03 must use the Stream-based
/// `pull_blob_stream` API and a manual sync pump instead of the
/// `AsyncWrite` sink shape (RESEARCH Open-Q #4 pivot).
#[test]
fn spike_a3_sync_io_bridge_under_current_thread() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all() // includes blocking pool — required by SyncIoBridge
        .build()
        .expect("build current_thread runtime");

    rt.block_on(async {
        // Wrap an async writer (Vec<u8> implements tokio::io::AsyncWrite)
        // in SyncIoBridge to expose it as a sync std::io::Write.
        let inner: Vec<u8> = Vec::new();
        let bridge = SyncIoBridge::new(inner);
        // Move the bridge into spawn_blocking and drive it synchronously.
        let recovered = tokio::task::spawn_blocking(move || {
            let mut bridge = bridge;
            bridge
                .write_all(b"hello via bridge")
                .expect("sync write_all");
            bridge.shutdown().expect("shutdown bridge");
            bridge.into_inner()
        })
        .await
        .expect("blocking task joined");
        assert_eq!(recovered, b"hello via bridge");
    });
}

/// SPIKE A3 negative control: confirm `SyncIoBridge` ALSO works under
/// the multi-thread runtime. If only this passes (and current_thread
/// fails), the design pivot is "use rt-multi-thread for the throwaway
/// runtime" — but that violates the Phase 41/42 single-thread isolation
/// invariant. Document that finding here.
#[test]
fn spike_a3_sync_io_bridge_under_multi_thread_control() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi_thread runtime");
    rt.block_on(async {
        let inner: Vec<u8> = Vec::new();
        let bridge = SyncIoBridge::new(inner);
        let recovered = tokio::task::spawn_blocking(move || {
            let mut bridge = bridge;
            bridge.write_all(b"control").expect("sync write_all");
            bridge.shutdown().expect("shutdown bridge");
            bridge.into_inner()
        })
        .await
        .expect("blocking task joined");
        assert_eq!(recovered, b"control");
    });
}

/// SPIKE A5: `zstd::stream::write::Decoder` works as a sync
/// `std::io::Write` sink (we feed it compressed bytes; it writes
/// decompressed bytes to its inner sink).
///
/// If this fails, Plan 04's pipeline composition must use
/// `zstd::Decoder<R: Read>` driven from the OPPOSITE direction (read
/// from the streamed bytes rather than write into the decoder).
#[test]
fn spike_a5_zstd_write_decoder_sink_shape() {
    // A5 resolved: we adopted pure-Rust ruzstd (Read-pull decoder + a buffered
    // Write adapter) to avoid linking C libzstd; ZstdDecodeWriter in
    // pichi::cmd::streaming_sink is the production form. This asserts the
    // underlying ruzstd round-trip.
    let original: &[u8] = b"the quick brown fox jumps over the lazy dog";
    let compressed =
        ruzstd::encoding::compress_to_vec(original, ruzstd::encoding::CompressionLevel::Fastest);
    let mut decoder =
        ruzstd::decoding::StreamingDecoder::new(compressed.as_slice()).expect("decoder");
    let mut decoded: Vec<u8> = Vec::new();
    std::io::copy(&mut decoder, &mut decoded).expect("decode");
    assert_eq!(decoded, original, "decoded bytes must match original");
}

// SPIKE A2 (oci-client TokenCache mid-pull 401 retry): documented as a
// follow-up integration test in Plan 06 against zot+htpasswd. Cannot run
// here without docker. We DOCUMENT the spike's plan in this comment so
// Plan 06's cmd_pull_bearer integration test inherits the design
// rationale:
//
//   1. Configure zot with htpasswd auth + JWT TTL = 5 seconds.
//   2. Push a >= 50MiB blob (pre-loaded into zot).
//   3. pichi pull with throttled stream (tc qdisc add dev lo … rate 5mbit)
//      so the stream takes >= 10 seconds.
//   4. Assert pull succeeds. If it fails with 401 mid-stream, oci-client's
//      TokenCache does NOT cover this case and Plan 03 must add explicit
//      retry-from-Range logic.
//
// PLAN 06 OWNS this test. Plan 01 documents A2 here so reviewers see the gap.
