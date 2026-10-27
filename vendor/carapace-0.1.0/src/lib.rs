// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! carapace — assembly and optional image conversion for carapace chains.
//!
//! Assembles a cryptographically-bound chain of content-addressable
//! block-device scutes into a unified, integrity-protected,
//! read-only device anchored by a single trusted root. Given a trusted
//! chain root, this library walks the partitions the kernel exposes via
//! `/sys/class/block/*/uevent` (PARTUUID lookup), validates every scute's
//! parameters against the RDP whitelist, and builds the dm-verity +
//! dm-snapshot stack. The on-disk format is specified in `docs/spec.md`; this
//! CLI is read-only; the optional `image` feature adds userspace conversion.
//!
//! Image conversion is portable across Linux, macOS, and Windows. Discovery,
//! assembly, and the CLI require Linux.
//!
//! # Library API
//!
//! - `Chain` — an ordered list of scutes (base → top) plus a trusted root.
//! - `Scute` — open data/hash files for one scute.
//! - `Assemble::assemble` — build the dm stack and return a [`std::fs::File`].
//! - `FromGpt` — discover a `Chain` by PARTUUID walk (feature `gpt`).
//! - `AsyncFromGpt` — Tokio discovery (features `gpt` + `tokio`).
//! - `image::Plan` / `image::AsyncPlan` — planned base-scute readers
//!   (feature `image`, plus `tokio` for asynchronous conversion).
//!
//! Linux-only re-export: `DevId`.

#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod assemble;
#[cfg(all(target_os = "linux", feature = "gpt"))]
mod chain;
#[cfg(feature = "image")]
pub mod image;
#[cfg(all(target_os = "linux", feature = "gpt"))]
mod partition;
#[cfg(target_os = "linux")]
mod snapshot;
#[cfg(target_os = "linux")]
mod util;
#[cfg(target_os = "linux")]
mod verity;

#[cfg(target_os = "linux")]
pub use devmap_linux::DevId;

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;

/// One scute's open data and hash inputs.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Scute {
    /// Open, readable block device or regular file containing the data.
    pub data: File,
    /// Open, readable block device or regular file containing the hash tree.
    pub hash: File,
}

/// An ordered chain of scutes, base → top, plus the operator-supplied
/// trusted root hash. Produced by `FromGpt::from_gpt` or constructed
/// directly from caller-opened files. The library imposes no chain-depth limit.
/// Inputs remain open until the chain is
/// dropped. Assembly rereads their headers using positional I/O; it neither
/// trusts discovery-time validation nor changes the input file offsets.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Chain {
    /// The trusted chain root (operator-supplied, out-of-band).
    pub root: Box<[u8]>,
    /// Scutes in BASE → TOP order.
    pub scutes: Vec<Scute>,
}

/// Assemble a chain into a read-only block device.
///
/// Import this trait for `.assemble()`. This operation blocks; async callers
/// should run it on their runtime's blocking pool.
#[cfg(target_os = "linux")]
pub trait Assemble {
    /// Returns an open device file at offset zero. Deferred removal is already
    /// scheduled: the device remains alive while this file, a clone, a mount,
    /// or another kernel holder retains it. Closing the last holder permits
    /// teardown of the stack. Device identity is available through metadata.
    fn assemble(&self) -> io::Result<std::fs::File>;
}

#[cfg(target_os = "linux")]
impl Assemble for Chain {
    fn assemble(&self) -> io::Result<std::fs::File> {
        self.assemble_sync()
    }
}

/// Discover a [`Chain`] from GPT partitions visible to the kernel, given
/// a trusted root hash. Enabled by the `gpt` feature (default on).
#[cfg(all(target_os = "linux", feature = "gpt"))]
pub trait FromGpt: Sized {
    /// Walk the PARTUUID chain starting from `root`, validate parameters,
    /// and return an ordered [`Chain`] (base → top).
    fn from_gpt(root: &[u8]) -> io::Result<Self>;
}

#[cfg(all(target_os = "linux", feature = "gpt"))]
impl FromGpt for Chain {
    fn from_gpt(root: &[u8]) -> io::Result<Self> {
        let map = partition::PartitionMap::scan()?;
        let scutes = chain::walk_chain(root, &map)?;
        Ok(Chain {
            root: root.to_vec().into_boxed_slice(),
            scutes,
        })
    }
}

/// Discover a chain through Tokio's blocking pool.
///
/// Import this trait for `Chain::from_gpt(root).await`. If [`FromGpt`] is also
/// in scope, use `<Chain as AsyncFromGpt>::from_gpt(root)`. Requires a Tokio
/// runtime; an unpolled future performs no discovery. Once started, discovery
/// may finish after the future is dropped. The root is copied for the worker.
#[cfg(all(target_os = "linux", feature = "gpt", feature = "tokio"))]
pub trait AsyncFromGpt: Sized {
    /// Find the kernel-visible GPT partitions, ordered base to top.
    fn from_gpt(root: &[u8]) -> impl std::future::Future<Output = io::Result<Self>> + Send + '_;
}

#[cfg(all(target_os = "linux", feature = "gpt", feature = "tokio"))]
impl AsyncFromGpt for Chain {
    async fn from_gpt(root: &[u8]) -> io::Result<Self> {
        let root = root.to_vec();
        blocking(move || <Self as FromGpt>::from_gpt(&root)).await
    }
}

// Execution boundary for discovery, preserving io errors.
#[cfg(all(target_os = "linux", feature = "gpt", feature = "tokio"))]
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|error| io::Error::other(format!("Tokio runtime required: {error}")))?;
    runtime
        .spawn_blocking(operation)
        .await
        .map_err(|error| io::Error::other(format!("carapace worker failed: {error}")))?
}

#[cfg(all(test, target_os = "linux", feature = "gpt", feature = "tokio"))]
mod async_tests {
    use super::*;
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn unpolled_work_does_not_start() {
        let (tx, rx) = mpsc::channel();
        let future = blocking(move || {
            tx.send(()).unwrap();
            Ok(())
        });
        drop(future);
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn missing_runtime_is_an_io_error() {
        let mut future = Box::pin(blocking(|| Ok(())));
        let mut cx = Context::from_waker(Waker::noop());
        let Poll::Ready(Err(error)) = future.as_mut().poll(&mut cx) else {
            panic!("expected runtime error")
        };
        assert!(error.to_string().contains("Tokio runtime required"));
    }

    #[test]
    fn worker_errors_preserve_io_kind_and_report_panics() {
        let rt = runtime();
        let error = rt
            .block_on(blocking::<()>(|| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let error = rt
            .block_on(blocking::<()>(|| panic!("worker failure")))
            .unwrap_err();
        assert!(error.to_string().contains("carapace worker failed"));
    }

    #[test]
    fn cancelled_wait_drops_the_workers_unclaimed_result() {
        struct OnDrop(mpsc::Sender<()>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
            }
        }
        let rt = runtime();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let mut future = Box::pin(blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(OnDrop(dropped_tx))
        }));
        rt.block_on(std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(future);
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[cfg(all(target_os = "linux", feature = "gpt"))]
    #[test]
    fn discovery_errors_match_and_future_is_send() {
        fn check(_: impl Future<Output = io::Result<Chain>> + Send) {}
        check(<Chain as AsyncFromGpt>::from_gpt(&[]));
        let sync = <Chain as FromGpt>::from_gpt(&[]).unwrap_err();
        let asynchronous = runtime()
            .block_on(<Chain as AsyncFromGpt>::from_gpt(&[]))
            .unwrap_err();
        assert_eq!(sync.kind(), asynchronous.kind());
        assert_eq!(sync.to_string(), asynchronous.to_string());
    }
}

// Shared format geometry for image conversion and Linux assembly.
#[cfg(any(target_os = "linux", feature = "image"))]
pub(crate) const ZERO_COUNT_SECTORS: u64 = 64u64 * 1024 * 1024 * 1024 / 512;
