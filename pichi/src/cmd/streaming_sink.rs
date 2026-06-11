// SPDX-License-Identifier: Apache-2.0

//! Phase 44 Plan 04 Task 1: streaming-pipeline writer adapters used by
//! `cmd::pull::run` to compose the per-layer pipeline.
//!
//! The five adapters are:
//!
//! 1. [`TeeWriter`] — split a write into two branches (Pitfall 5/12 sketch:
//!    outer `Tee(Sha256-of-compressed, ZstdDecode -> Tee(Sha256-of-decompressed,
//!    VerityFeed -> LimitWriter -> tempfile))`).
//! 2. [`DigestWriter`] — accumulate a SHA-256 hash over every byte written.
//! 3. [`ZstdDecodeWriter`] — wrap [`zstd::stream::write::Decoder`] (Plan 01
//!    SPIKE A5 confirmed shape works as a sync sink). MUST be `finish()`-ed
//!    before the inner sha256 hasher is finalised (Pitfall 5).
//! 4. [`VerityFeedWriter`] — buffer writes into 4 KiB blocks and fire a
//!    callback per full block (mirrors `tools/import/src/lib.rs` lines
//!    215-238). On `finish()`, zero-pads any partial trailing block before
//!    firing the final feed.
//! 5. [`LimitWriter`] — error if the cumulative write count exceeds a cap
//!    (compressed-bomb defence per RESEARCH §"Known Threat Patterns" line
//!    1620).
//!
//! Tear-down ordering (Pitfall 5): the orchestrator drops the OUTERMOST
//! sink first to trigger ZstdDecodeWriter's flush, then finalises the
//! hashers. The `pipeline_composition_end_to_end` test below documents
//! this sequence as a regression guard.

// Several adapter methods (`into_inner`, `finish`) are part of the documented
// API contract but the v0.8 cmd::pull pipeline relies on `Drop` for finalisation
// (LayerCapture::finalize_into drops the outermost sink which cascades through
// the chain). Keeping the methods + `DigestWriter` available for callers that
// need explicit tear-down ordering (the `pipeline_composition_end_to_end`
// test exercises them) without breaking the workspace `dead_code = "deny"`
// rule for the parts that are not yet reached from main.
#![allow(dead_code)]
// These adapters wrap closures (`VerityFeedWriter`), the non-Debug
// `zstd::stream::write::Decoder` (`ZstdDecodeWriter`), and arbitrary inner
// writers — a `Debug` impl would be meaningless plumbing noise.
#![allow(missing_debug_implementations)]

use std::io::{self, Write};

use sha2::{Digest as _, Sha256};

/// `TeeWriter<A, B>` writes every byte to both branches.
///
/// Per write, both branches are driven via `write_all`; if either fails the
/// returned error propagates and the partial state of the other branch is
/// undefined. Drop order of the two fields is the declaration order
/// (`a` first, then `b`) per Rust's drop semantics; the orchestrator should
/// rely on [`Self::into_inner`] for explicit tear-down ordering rather than
/// the implicit Drop.
pub struct TeeWriter<A: Write, B: Write> {
    a: A,
    b: B,
}

impl<A: Write, B: Write> TeeWriter<A, B> {
    /// Construct a new `TeeWriter` from two sinks.
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }

    /// Recover both branches. Caller decides the tear-down order — Plan 04's
    /// `LayerCapture::finalize_into` drops the outermost sink first to
    /// trigger ZstdDecodeWriter's flush BEFORE finalising any hashers.
    pub fn into_inner(self) -> (A, B) {
        (self.a, self.b)
    }
}

impl<A: Write, B: Write> Write for TeeWriter<A, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.a.write_all(buf)?;
        self.b.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.a.flush()?;
        self.b.flush()
    }
}

/// `DigestWriter` accumulates a SHA-256 hash over every byte fed to its
/// `Write` impl. The `Write::write` calls always succeed and report the full
/// `buf.len()` consumed; `flush` is a no-op.
///
/// Call [`Self::finalize`] to consume the writer and read out the digest +
/// total bytes seen.
pub struct DigestWriter {
    hasher: Sha256,
    bytes_written: u64,
}

impl Default for DigestWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl DigestWriter {
    /// Construct a new `DigestWriter` with a fresh sha256 hasher.
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }

    /// Consume the writer and return the (digest, byte-count) pair.
    pub fn finalize(self) -> ([u8; 32], u64) {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.hasher.finalize());
        (out, self.bytes_written)
    }
}

impl Write for DigestWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.bytes_written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `ZstdDecodeWriter<W>` wraps [`zstd::stream::write::Decoder`]. Bytes
/// written to this sink are decoded as zstd-compressed input and the
/// decompressed output flows into the inner writer `W`.
///
/// **Pitfall 5 ordering invariant:** call [`Self::finish`] (or let `Drop`
/// do it) BEFORE finalising any sha256 hasher that is downstream of the
/// decoder. `finish` calls the decoder's internal `flush()` which emits
/// any final residue; without it the inner hasher misses the last
/// compressed-block worth of decoded bytes.
///
/// Per Plan 01 SPIKE A5: `zstd::stream::write::Decoder<'static, W>` is the
/// confirmed sync-sink shape; the spike test established that bytes written
/// to the decoder show up in the inner sink exactly as decoded.
pub struct ZstdDecodeWriter<W: Write> {
    decoder: Option<zstd::stream::write::Decoder<'static, W>>,
}

impl<W: Write> ZstdDecodeWriter<W> {
    /// Construct a new `ZstdDecodeWriter` wrapping `inner`.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` if the zstd decoder context cannot
    /// be initialised.
    pub fn new(inner: W) -> io::Result<Self> {
        Ok(Self {
            decoder: Some(zstd::stream::write::Decoder::new(inner)?),
        })
    }

    /// Flush the decoder (emit any trailing decompressed bytes), recover
    /// the inner writer, and return it. Consumes the decoder.
    ///
    /// # Errors
    /// Propagates any flush error from the underlying decoder or sink.
    pub fn finish(mut self) -> io::Result<W> {
        let mut decoder = self
            .decoder
            .take()
            .ok_or_else(|| io::Error::other("ZstdDecodeWriter::finish called twice"))?;
        decoder.flush()?;
        Ok(decoder.into_inner())
    }
}

impl<W: Write> Write for ZstdDecodeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.decoder.as_mut() {
            Some(d) => d.write(buf),
            None => Err(io::Error::other(
                "ZstdDecodeWriter: writer used after finish",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.decoder.as_mut() {
            Some(d) => d.flush(),
            None => Ok(()),
        }
    }
}

impl<W: Write> Drop for ZstdDecodeWriter<W> {
    fn drop(&mut self) {
        // Best-effort flush on drop so a caller that forgets `finish()` still
        // gets the final residue into the inner sink. Ignore errors — Drop
        // can't propagate them; production code should call `finish()`
        // explicitly to surface flush failures.
        if let Some(d) = self.decoder.as_mut() {
            let _ = d.flush();
        }
    }
}

/// Default verity data block size (Phase 42 D-06 locked default).
pub const VERITY_BLOCK_SIZE: usize = 4096;

/// `VerityFeedWriter<W, F>` buffers writes into [`VERITY_BLOCK_SIZE`]-byte
/// blocks and fires `feed(&block)` per full block. The bytes also pass
/// through to `inner` so a downstream sink (e.g. tempfile) sees the entire
/// stream.
///
/// On [`Self::finish`], any partial trailing block is zero-padded to
/// `VERITY_BLOCK_SIZE` and `feed` is invoked one final time. This mirrors
/// the per-block hashing behaviour in `tools/import/src/lib.rs` lines
/// 215-238.
pub struct VerityFeedWriter<W: Write, F: FnMut(&[u8]) -> io::Result<()>> {
    inner: W,
    feed: F,
    buf: Vec<u8>,
}

impl<W: Write, F: FnMut(&[u8]) -> io::Result<()>> VerityFeedWriter<W, F> {
    /// Construct a new `VerityFeedWriter`. The buffer is pre-allocated to
    /// [`VERITY_BLOCK_SIZE`] capacity so steady-state writes do not realloc.
    pub fn new(inner: W, feed: F) -> Self {
        Self {
            inner,
            feed,
            buf: Vec::with_capacity(VERITY_BLOCK_SIZE),
        }
    }

    /// Flush any pending bytes (zero-padded to a full block) through `feed`,
    /// flush the inner sink, and return it.
    ///
    /// # Errors
    /// Propagates errors from the feed callback or the inner sink.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.buf.is_empty() {
            // Zero-pad the trailing partial block so the caller's verity
            // hasher sees a full VERITY_BLOCK_SIZE-byte input.
            let pad = VERITY_BLOCK_SIZE - self.buf.len();
            self.buf.extend(std::iter::repeat_n(0u8, pad));
            (self.feed)(&self.buf)?;
            self.buf.clear();
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write, F: FnMut(&[u8]) -> io::Result<()>> Write for VerityFeedWriter<W, F> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let original_len = buf.len();
        // Pass-through to inner first so a downstream LimitWriter / tempfile
        // sees the bytes in the same order they arrived.
        self.inner.write_all(buf)?;
        while !buf.is_empty() {
            let need = VERITY_BLOCK_SIZE - self.buf.len();
            let take = need.min(buf.len());
            self.buf.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if self.buf.len() == VERITY_BLOCK_SIZE {
                (self.feed)(&self.buf)?;
                self.buf.clear();
            }
        }
        Ok(original_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `LimitWriter<W>` errors if the cumulative bytes written exceed `cap`.
/// Used as a defence against zstd-bombs: a few KB of compressed input that
/// expands to terabytes of decompressed output (RESEARCH §"Known Threat
/// Patterns" line 1620).
pub struct LimitWriter<W: Write> {
    inner: W,
    remaining: u64,
}

impl<W: Write> LimitWriter<W> {
    /// Construct a new `LimitWriter` that allows `cap` bytes through.
    pub fn new(inner: W, cap: u64) -> Self {
        Self {
            inner,
            remaining: cap,
        }
    }

    /// Recover the inner writer (consumes the limiter).
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for LimitWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let buf_len = buf.len() as u64;
        if buf_len > self.remaining {
            return Err(io::Error::other(format!(
                "decompressed-bytes cap exceeded (remaining={remaining} more={more})",
                remaining = self.remaining,
                more = buf_len
            )));
        }
        self.inner.write_all(buf)?;
        self.remaining -= buf_len;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adapter test 1: TeeWriter mirrors writes to both branches.
    #[test]
    fn tee_writer_passes_bytes_to_both_branches() {
        let a: Vec<u8> = Vec::new();
        let b: Vec<u8> = Vec::new();
        let mut tee = TeeWriter::new(a, b);
        tee.write_all(b"hello").unwrap();
        let (a, b) = tee.into_inner();
        assert_eq!(a, b"hello");
        assert_eq!(b, b"hello");
    }

    /// Adapter test 2: DigestWriter computes the correct sha256 of input.
    #[test]
    fn digest_writer_finalizes_to_correct_sha256() {
        let mut dw = DigestWriter::new();
        dw.write_all(b"abc").unwrap();
        let (digest, n) = dw.finalize();
        assert_eq!(n, 3);
        // Known sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            hex::encode(digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Adapter test 3: ZstdDecodeWriter decodes a pre-encoded payload to the
    /// original bytes.
    #[test]
    fn zstd_decode_writer_streams_decompressed_bytes() {
        let original = b"the quick brown fox jumps over the lazy dog";
        let compressed = zstd::encode_all(&original[..], 0).unwrap();
        let mut zdw = ZstdDecodeWriter::new(Vec::<u8>::new()).unwrap();
        zdw.write_all(&compressed).unwrap();
        let inner = zdw.finish().unwrap();
        assert_eq!(inner, original);
    }

    /// Adapter test 4: VerityFeedWriter chunks writes into 4 KiB blocks
    /// regardless of how the caller's writes are sliced.
    #[test]
    fn verity_feed_writer_chunks_to_4kib_blocks() {
        let calls = std::cell::RefCell::new(Vec::<usize>::new());
        let inner: Vec<u8> = Vec::new();
        // 12 KiB of data; expect exactly 3 feed calls of 4 KiB each.
        let data = vec![0xABu8; 12 * 1024];
        {
            let mut vf = VerityFeedWriter::new(inner, |block: &[u8]| -> io::Result<()> {
                calls.borrow_mut().push(block.len());
                Ok(())
            });
            // Write in awkward chunk sizes to exercise the inner buffer logic.
            vf.write_all(&data[..3000]).unwrap();
            vf.write_all(&data[3000..7000]).unwrap();
            vf.write_all(&data[7000..]).unwrap();
            let _ = vf.finish().unwrap();
        }
        assert_eq!(*calls.borrow(), vec![4096, 4096, 4096]);
    }

    /// Adapter test 5: LimitWriter errors past its cap.
    #[test]
    fn limit_writer_errors_past_cap() {
        let mut lw = LimitWriter::new(Vec::<u8>::new(), 4);
        lw.write_all(b"abcd").unwrap();
        let err = lw.write_all(b"e").unwrap_err();
        assert!(
            err.to_string().contains("decompressed-bytes cap exceeded"),
            "wrong error: {err}"
        );
    }

    /// Adapter test 6 — Pitfall 5/12 regression guard.
    ///
    /// Compose: outer-Tee(outer-DigestWriter, ZstdDecode(inner-Tee(inner-DigestWriter,
    /// VerityFeed -> dest-Vec))). Feed a synthetic compressed payload, then
    /// tear down INSIDE-OUT (drop sink first, then finalise hashers) to mirror
    /// the ordering `LayerCapture::finalize_into` enforces in cmd::pull.
    /// Asserts:
    ///   (a) outer digest = sha256(compressed bytes)
    ///   (b) inner digest = sha256(decompressed bytes)
    ///   (c) the decompressed bytes flow through verbatim to the dest Vec.
    #[test]
    fn pipeline_composition_end_to_end() {
        use std::sync::{Arc, Mutex};
        let original = b"the quick brown fox";
        let compressed = zstd::encode_all(&original[..], 0).unwrap();

        // Hashers shared via Arc<Mutex<>> so the test (analogous to
        // LayerCapture in cmd::pull) can finalise them after the outer sink
        // is dropped.
        let outer_hasher = Arc::new(Mutex::new(Sha256::new()));
        let inner_hasher = Arc::new(Mutex::new(Sha256::new()));

        struct SharedHasher(Arc<Mutex<Sha256>>);
        impl Write for SharedHasher {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().update(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        // Build the dest sink that captures decompressed bytes.
        let dest: Vec<u8> = Vec::new();

        // Verity feed — for the regression guard we don't need an actual
        // verity tree; just count bytes seen.
        let verity_seen = Arc::new(Mutex::new(0usize));
        let verity_seen_for_cb = Arc::clone(&verity_seen);
        let verity_sink = VerityFeedWriter::new(dest, move |block: &[u8]| -> io::Result<()> {
            *verity_seen_for_cb.lock().unwrap() += block.len();
            Ok(())
        });

        // Inner Tee splits the decompressed stream.
        let inner_hasher_writer = SharedHasher(Arc::clone(&inner_hasher));
        let inner_tee = TeeWriter::new(inner_hasher_writer, verity_sink);
        // Zstd decode wraps the inner Tee.
        let zdw = ZstdDecodeWriter::new(inner_tee).unwrap();
        // Outer Tee splits compressed-side hashing from decode-side.
        let outer_hasher_writer = SharedHasher(Arc::clone(&outer_hasher));
        let mut outer = TeeWriter::new(outer_hasher_writer, zdw);

        // Feed the compressed bytes.
        outer.write_all(&compressed).unwrap();

        // INSIDE-OUT tear-down: drop the outermost Tee to release branches.
        // The outer-side hasher is dropped (end of write) and the inner-side
        // ZstdDecodeWriter sees its drop, which our explicit-finish-or-drop
        // shape flushes.
        let (outer_hasher_writer, zdw) = outer.into_inner();
        // Explicit drop of outer hasher branch (it has no resources, but
        // mirrors the inside-out sequence).
        drop(outer_hasher_writer);
        // Finish the zstd decode (Pitfall 5: BEFORE finalising the inner
        // hasher).
        let inner_tee_recovered = zdw.finish().unwrap();
        let (inner_hasher_writer_recovered, verity_sink_recovered) =
            inner_tee_recovered.into_inner();
        drop(inner_hasher_writer_recovered);
        let dest_final = verity_sink_recovered.finish().unwrap();

        // Now finalise both hashers (post-Pitfall-5 sequence).
        // Use Arc::try_unwrap to pull the Mutex back; since we dropped both
        // SharedHasher writers above, the Arc has only one strong ref left
        // (this scope).
        let outer_hash = std::sync::Arc::try_unwrap(outer_hasher)
            .ok()
            .expect("outer hasher arc still has refs")
            .into_inner()
            .unwrap()
            .finalize();
        let inner_hash = std::sync::Arc::try_unwrap(inner_hasher)
            .ok()
            .expect("inner hasher arc still has refs")
            .into_inner()
            .unwrap()
            .finalize();

        // (a) outer digest = sha256(compressed bytes).
        let expected_outer = {
            let mut h = Sha256::new();
            h.update(&compressed);
            h.finalize()
        };
        assert_eq!(
            outer_hash.as_slice(),
            expected_outer.as_slice(),
            "outer (compressed-side) digest must equal sha256(compressed)"
        );

        // (b) inner digest = sha256(original bytes).
        let expected_inner = {
            let mut h = Sha256::new();
            h.update(&original[..]);
            h.finalize()
        };
        assert_eq!(
            inner_hash.as_slice(),
            expected_inner.as_slice(),
            "inner (decompressed-side) digest must equal sha256(decompressed)"
        );

        // (c) dest contains the decompressed bytes verbatim.
        assert_eq!(
            dest_final, original,
            "dest sink must contain the verbatim decompressed bytes"
        );

        // verity feed must have seen at least the original-byte count
        // (zero-padded to a full 4 KiB block for the trailing partial).
        assert!(
            *verity_seen.lock().unwrap() >= original.len(),
            "verity feed callback must have seen at least the decompressed payload"
        );
    }
}
