// SPDX-License-Identifier: Apache-2.0

//! VirtioDevice trait defining the device contract for transport layers.

use vm_memory::GuestMemoryMmap;

use crate::kick::Kick;
use crate::queue::Queue;

use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Transport-resolved activation inputs for one virtio device.
#[derive(Debug)]
pub struct VirtioActivate {
    pub mem: GuestMemoryMmap,
    pub queues: Vec<Queue>,
    pub queue_evts: Vec<Kick>,
    pub host: Arc<dyn VirtioDeviceHost>,
}

impl VirtioActivate {
    pub fn new(mem: GuestMemoryMmap, queues: Vec<Queue>, queue_evts: Vec<Kick>) -> Self {
        Self {
            mem,
            queues,
            queue_evts,
            host: Arc::new(ThreadDeviceHost),
        }
    }

    pub fn with_host(
        mem: GuestMemoryMmap,
        queues: Vec<Queue>,
        queue_evts: Vec<Kick>,
        host: Arc<dyn VirtioDeviceHost>,
    ) -> Self {
        Self {
            mem,
            queues,
            queue_evts,
            host,
        }
    }
}

/// Errors returned by [`VirtioDevice::activate`].
#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    /// The device received an invalid configuration.
    #[error("invalid device configuration: {0}")]
    InvalidConfig(String),

    /// An internal I/O error occurred during activation.
    #[error("activation I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Error returned when joining a running virtio device host.
#[derive(Debug, thiserror::Error)]
pub enum DeviceJoinError {
    /// A worker thread panicked.
    #[error("virtio device worker panicked")]
    Panicked,

    /// Device-specific worker failure.
    #[error("virtio device worker failed: {0}")]
    Worker(String),
}

/// Token passed to a virtio device worker started by the transport host.
#[derive(Clone)]
pub struct VirtioRunToken {
    is_shutdown_requested: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl std::fmt::Debug for VirtioRunToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioRunToken").finish_non_exhaustive()
    }
}

impl VirtioRunToken {
    pub fn from_fn(is_shutdown_requested: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            is_shutdown_requested: Arc::new(is_shutdown_requested),
        }
    }

    pub fn is_shutdown_requested(&self) -> bool {
        (self.is_shutdown_requested)()
    }
}

/// Host-side execution service for virtio device workers.
pub trait VirtioDeviceHost: Send + Sync + std::fmt::Debug {
    fn spawn(
        &self,
        run: Box<dyn FnOnce(VirtioRunToken) -> Result<(), DeviceJoinError> + Send>,
    ) -> Result<VirtioDeviceHandle, ActivateError>;

    fn adopt_process(&self, child: Child) -> Result<VirtioDeviceHandle, ActivateError>;
}

/// Compatibility host that runs virtio device workers as local threads.
#[derive(Debug)]
pub struct ThreadDeviceHost;

impl VirtioDeviceHost for ThreadDeviceHost {
    fn spawn(
        &self,
        run: Box<dyn FnOnce(VirtioRunToken) -> Result<(), DeviceJoinError> + Send>,
    ) -> Result<VirtioDeviceHandle, ActivateError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let token_shutdown = Arc::clone(&shutdown);
        let token = VirtioRunToken::from_fn(move || token_shutdown.load(Ordering::Acquire));
        let join = thread::spawn(move || run(token));
        Ok(VirtioDeviceHandle::new(
            move || shutdown.store(true, Ordering::Release),
            move || match join.join() {
                Ok(result) => result,
                Err(_) => Err(DeviceJoinError::Panicked),
            },
        ))
    }

    fn adopt_process(&self, child: Child) -> Result<VirtioDeviceHandle, ActivateError> {
        Ok(process_handle(child))
    }
}

fn process_handle(child: Child) -> VirtioDeviceHandle {
    let child = Arc::new(Mutex::new(Some(child)));
    let shutdown_child = Arc::clone(&child);
    let join_child = Arc::clone(&child);
    VirtioDeviceHandle::new(
        move || {
            if let Some(mut child) = shutdown_child
                .lock()
                .expect("virtio process child poisoned")
                .take()
            {
                let _ = terminate_process(&mut child);
            }
        },
        move || {
            if let Some(mut child) = join_child
                .lock()
                .expect("virtio process child poisoned")
                .take()
            {
                terminate_process(&mut child)
                    .map_err(|e| DeviceJoinError::Worker(e.to_string()))?;
            }
            Ok(())
        },
    )
}

fn terminate_process(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    child.kill()?;
    child.wait()?;
    Ok(())
}

/// Runtime handle for workers started by one virtio device activation.
pub struct VirtioDeviceHandle {
    shutdown: Option<Box<dyn FnOnce() + Send>>,
    join: Option<Box<dyn FnOnce() -> Result<(), DeviceJoinError> + Send>>,
}

impl std::fmt::Debug for VirtioDeviceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioDeviceHandle").finish_non_exhaustive()
    }
}

impl VirtioDeviceHandle {
    pub fn new(
        shutdown: impl FnOnce() + Send + 'static,
        join: impl FnOnce() -> Result<(), DeviceJoinError> + Send + 'static,
    ) -> Self {
        Self {
            shutdown: Some(Box::new(shutdown)),
            join: Some(Box::new(join)),
        }
    }

    pub fn noop() -> Self {
        Self::new(|| {}, || Ok(()))
    }

    pub fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
    }

    pub fn join(mut self) -> Result<(), DeviceJoinError> {
        self.shutdown();
        if let Some(join) = self.join.take() {
            join()
        } else {
            Ok(())
        }
    }
}

impl Drop for VirtioDeviceHandle {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(join) = self.join.take() {
            if let Err(e) = join() {
                log_join_error(&e);
            }
        }
    }
}

fn log_join_error(e: &DeviceJoinError) {
    #[cfg(any(test, debug_assertions))]
    eprintln!("virtio device worker join failed during drop: {e}");
    #[cfg(not(any(test, debug_assertions)))]
    let _ = e;
}

/// Transport-agnostic virtio device contract.
///
/// Implementors define device-specific behaviour; the transport layer
/// (virtio-pci or virtio-mmio) handles capability layout, feature
/// negotiation, and queue setup before calling [`activate`](Self::activate).
pub trait VirtioDevice: Send {
    /// Virtio device type identifier (e.g. 1 = net, 2 = block, 3 = console).
    fn device_type(&self) -> u32;

    /// Number of virtqueues this device uses.
    fn num_queues(&self) -> usize;

    /// Maximum queue size for each virtqueue.
    fn queue_max_sizes(&self) -> &[u16];

    /// Device feature bits (including `VIRTIO_F_VERSION_1`).
    fn features(&self) -> u64;

    /// Activate the device with negotiated queues and notification eventfds.
    ///
    /// The transport calls this after feature negotiation and queue setup are
    /// complete. The device typically spawns an I/O thread that blocks on
    /// `queue_evts` and processes descriptors via the provided queues.
    ///
    /// `queue_evts` are [`Kick`]s: on Linux they wrap KVM-ioeventfd-driven
    /// eventfds; on macOS/HVF they are in-process condvar notifiers raised by
    /// the transport's MMIO notify path.
    fn activate(&mut self, activation: VirtioActivate)
    -> Result<VirtioDeviceHandle, ActivateError>;

    /// Read device-specific configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Write device-specific configuration space.
    fn write_config(&mut self, offset: u64, data: &[u8]);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn handle_shutdown_runs_once_before_join() {
        let stopped = Arc::new(AtomicBool::new(false));
        let shutdown_stopped = Arc::clone(&stopped);
        let join_stopped = Arc::clone(&stopped);
        let mut handle = VirtioDeviceHandle::new(
            move || shutdown_stopped.store(true, Ordering::Release),
            move || {
                assert!(join_stopped.load(Ordering::Acquire));
                Ok(())
            },
        );

        handle.shutdown();
        assert!(stopped.load(Ordering::Acquire));
        handle.join().expect("joined");
    }

    #[test]
    fn noop_handle_joins() {
        VirtioDeviceHandle::noop().join().expect("noop join");
    }

    #[test]
    fn thread_host_reports_shutdown_to_worker() {
        let host = ThreadDeviceHost;
        let handle = host
            .spawn(Box::new(|token| {
                while !token.is_shutdown_requested() {
                    std::thread::yield_now();
                }
                Ok(())
            }))
            .expect("spawned");
        let mut handle = handle;
        handle.shutdown();
        handle.join().expect("joined");
    }
}
