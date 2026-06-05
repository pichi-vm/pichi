// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Shared vhost-user backend harness for dillo device backends.
//!
//! This crate provides the common infrastructure for running a vhost-user backend
//! over an inherited socket file descriptor. The single `unsafe` block for `from_raw_fd`
//! is isolated here per the mutual-distrust isolation requirement (ISOL-07).
//!
//! # Architecture
//!
//! Device backends receive a connected socketpair fd from the VMM parent process (inherited
//! across `exec`). This module converts that raw fd into a [`UnixStream`], wraps the
//! [`VhostUserBackendMut`] implementor in a [`BackendMutAdapter`], and drives the vhost-user
//! protocol loop until the frontend (VMM) disconnects.
//!
//! [`VhostUserDaemon`] from `vhost-user-backend` requires a named socket path (it cannot
//! accept an already-connected `UnixStream`). Therefore `run_backend` uses
//! [`BackendReqHandler::from_stream`] directly, bridged to the high-level
//! [`VhostUserBackendMut`] trait via [`BackendMutAdapter`].

#![deny(unsafe_code)]

use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use thiserror::Error;
use vhost::vhost_user::message::{
    VhostUserConfigFlags, VhostUserInflight, VhostUserMemoryRegion, VhostUserSharedMsg,
    VhostUserSingleMemoryRegion, VhostUserVringAddrFlags, VhostUserVringState,
};
use vhost::vhost_user::{BackendReqHandler, GpuBackend, Result as VhostResult};
use vm_memory::{GuestAddress, GuestMemoryAtomic, GuestMemoryMmap, GuestRegionMmap};

pub use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserProtocolFeatures,
    VhostUserVirtioFeatures,
};
pub use vhost_user_backend::{VhostUserBackendMut, VringRwLock, VringT};
pub use vmm_sys_util::epoll::EventSet;

/// Optional per-queue extra wakeup notifier.
///
/// Backends that need to self-trigger queue processing (without waiting for a
/// guest kick) can implement this trait to provide an extra eventfd for each
/// queue. The eventfd's write end is retained by the backend; the read end
/// (as a `File`) is returned here so the kick thread can also epoll-wait on it.
///
/// Default implementation returns `None` for all queues (no extra notifier).
pub trait QueueNotifier {
    /// Return a dup of the read end of the extra notifier eventfd for `queue_idx`,
    /// or `None` if no extra notifier is needed for that queue.
    fn extra_kick_notifier(&self, _queue_idx: usize) -> Option<File> {
        None
    }
}

/// Errors returned by [`run_backend`].
#[derive(Debug, Error)]
pub enum Error {
    /// A vhost-user protocol error occurred.
    #[error("vhost-user protocol error: {0}")]
    Protocol(vhost::vhost_user::Error),
}

// ---------------------------------------------------------------------------
// BackendMutAdapter: bridges VhostUserBackendMut → VhostUserBackendReqHandlerMut
// ---------------------------------------------------------------------------

/// Per-vring state tracked from the vhost-user protocol messages.
///
/// This tracks the raw protocol-level addresses needed to configure a
/// `VringRwLock` once guest memory is available.
#[derive(Default)]
struct VringState {
    /// Descriptor table guest physical address.
    desc_table: u64,
    /// Available ring guest physical address.
    avail_ring: u64,
    /// Used ring guest physical address.
    used_ring: u64,
    /// Number of queue entries.
    size: u32,
    /// Avail index base (returned by get_vring_base).
    base: u32,
    /// Whether the queue is enabled.
    enabled: bool,
    /// Call eventfd (KVM irqfd) — written by signal_used_queue() to inject MSI-X into guest.
    /// Set by set_vring_call(); applied to VringRwLock in configure_vring().
    call_fd: Option<File>,
}

/// Bridges a [`VhostUserBackendMut`] implementation to the low-level
/// [`vhost::vhost_user::VhostUserBackendReqHandlerMut`] trait.
///
/// `VhostUserDaemon` from `vhost-user-backend` cannot accept an already-connected
/// `UnixStream` (it requires a named socket path). This adapter wraps the high-level
/// `VhostUserBackendMut` backend and implements the full vhost-user protocol handler
/// interface for use with [`BackendReqHandler::from_stream`].
///
/// Memory updates (from `set_mem_table`) are forwarded to the backend via
/// `VhostUserBackendMut::update_memory`. Kick fds (from `set_vring_kick`) cause
/// per-queue epoll threads to be spawned that call `handle_event` when the guest
/// kicks a queue.
pub struct BackendMutAdapter<T: VhostUserBackendMut + 'static> {
    backend: Arc<Mutex<T>>,
    acked_features: u64,
    acked_protocol_features: u64,
    num_queues: usize,
    vrings: Vec<VringState>,
    /// Guest memory — set on first set_mem_table call.
    atomic_mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    /// Maximum queue size (from backend::max_queue_size), needed for VringRwLock creation.
    max_queue_size: u16,
    /// Configured VringRwLock instances — shared with kick threads.
    /// Each slot is `Some` once the corresponding queue is enabled with memory configured.
    vring_locks: Arc<Mutex<Vec<Option<VringRwLock>>>>,
}

impl<T: VhostUserBackendMut + 'static> std::fmt::Debug for BackendMutAdapter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendMutAdapter")
            .field("acked_features", &self.acked_features)
            .field("acked_protocol_features", &self.acked_protocol_features)
            .field("num_queues", &self.num_queues)
            .field("max_queue_size", &self.max_queue_size)
            .finish_non_exhaustive()
    }
}

impl<T: VhostUserBackendMut<Bitmap = (), Vring = VringRwLock> + QueueNotifier + Send + 'static>
    BackendMutAdapter<T>
{
    /// Create a new adapter wrapping `backend`.
    fn new(backend: Arc<Mutex<T>>) -> Self {
        let guard = backend.lock().unwrap();
        let num_queues = guard.num_queues();
        let max_queue_size = guard.max_queue_size() as u16;
        drop(guard);
        let vring_locks = Arc::new(Mutex::new(
            (0..num_queues)
                .map(|_| None)
                .collect::<Vec<Option<VringRwLock>>>(),
        ));
        Self {
            backend,
            acked_features: 0,
            acked_protocol_features: 0,
            num_queues,
            vrings: (0..num_queues).map(|_| VringState::default()).collect(),
            atomic_mem: None,
            max_queue_size,
            vring_locks,
        }
    }

    /// Build a GuestMemoryMmap from set_mem_table regions and update the backend.
    fn apply_mem_table(&mut self, ctx: &[VhostUserMemoryRegion], files: Vec<File>) {
        if ctx.len() != files.len() {
            log::error!(
                "set_mem_table: region count {} != file count {}",
                ctx.len(),
                files.len()
            );
            return;
        }
        let mut regions: Vec<GuestRegionMmap> = Vec::with_capacity(ctx.len());
        for (region, file) in ctx.iter().zip(files) {
            let mmap = match region.mmap_region::<()>(file) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("set_mem_table: mmap_region failed: {e}");
                    return;
                }
            };
            let Some(guest_region) =
                GuestRegionMmap::new(mmap, GuestAddress(region.guest_phys_addr))
            else {
                log::error!("set_mem_table: GuestRegionMmap::new failed");
                return;
            };
            regions.push(guest_region);
        }
        let mem = match GuestMemoryMmap::from_regions(regions) {
            Ok(m) => m,
            Err(e) => {
                log::error!("set_mem_table: from_regions failed: {e}");
                return;
            }
        };
        let atomic = GuestMemoryAtomic::new(mem);
        if let Err(e) = self.backend.lock().unwrap().update_memory(atomic.clone()) {
            log::error!("set_mem_table: backend update_memory failed: {e}");
        }
        self.atomic_mem = Some(atomic);
    }

    /// Create or update the `VringRwLock` for the given queue index.
    ///
    /// Called when both guest memory (from `set_mem_table`) and queue info
    /// (from `set_vring_addr`/`set_vring_num`) are available. The resulting
    /// lock is stored in `vring_locks` and passed to `handle_event` so backends
    /// can access virtqueue descriptors.
    fn configure_vring(&mut self, queue_idx: usize) {
        let Some(mem) = self.atomic_mem.clone() else {
            log::debug!("configure_vring[{queue_idx}]: no memory yet, skipping");
            return; // Memory not yet available
        };
        let Some(v) = self.vrings.get_mut(queue_idx) else {
            return;
        };

        let desc_table = v.desc_table;
        let avail_ring = v.avail_ring;
        let used_ring = v.used_ring;
        let vring_size = v.size;
        // Take the call fd out of VringState — it will be moved into the VringRwLock.
        // The vhost-user protocol sends set_vring_call once; taking it here is correct.
        let call_fd = v.call_fd.take();

        log::debug!(
            "configure_vring[{queue_idx}]: desc={desc_table:#x} avail={avail_ring:#x} used={used_ring:#x} size={vring_size} has_call={}",
            call_fd.is_some()
        );

        // Only configure if queue addresses are set (non-zero desc_table).
        if desc_table == 0 {
            log::debug!("configure_vring[{queue_idx}]: desc_table=0, skipping");
            return;
        }

        // Use the driver-negotiated queue size (set_vring_num) as the max for VringRwLock::new.
        // VringRwLock::new sets queue.max_size = queue.size = effective_size. The queue ring
        // layout is based on the size negotiated between driver and device, so the backend must
        // use the same size to correctly index descriptors.
        let effective_size = if vring_size > 0 {
            vring_size as u16
        } else {
            self.max_queue_size
        };

        match VringRwLock::new(mem, effective_size) {
            Ok(vring) => {
                use vhost_user_backend::VringT;
                vring.set_enabled(true);
                if let Err(e) = vring.set_queue_info(desc_table, avail_ring, used_ring) {
                    log::error!("configure_vring[{queue_idx}]: set_queue_info failed: {e}");
                    return;
                }
                // Mark the queue as ready so pop_descriptor_chain() can process
                // requests. The virtio-queue crate requires queue.ready = true
                // before iter() will return available descriptors; set_queue_info
                // only sets the ring addresses, not the ready flag.
                vring.set_queue_ready(true);
                // Set the call eventfd (KVM irqfd) so signal_used_queue() injects MSI-X
                // into the guest when the backend produces used descriptors.
                vring.set_call(call_fd);
                log::debug!("configure_vring[{queue_idx}]: vring configured and ready");
                if let Ok(mut locks) = self.vring_locks.lock() {
                    if let Some(slot) = locks.get_mut(queue_idx) {
                        *slot = Some(vring);
                        log::debug!("configure_vring[{queue_idx}]: vring stored in locks");
                    }
                }
            }
            Err(e) => {
                log::error!("configure_vring[{queue_idx}]: VringRwLock::new failed: {e}");
            }
        }
    }

    /// Spawn an epoll thread for the given queue that waits for kick events
    /// and calls `backend.handle_event(queue_idx, EventSet::IN, vrings, thread_id)`.
    ///
    /// If `extra_notifier` is `Some`, the thread also waits on that fd. This allows
    /// backend threads (e.g., an RX stdin reader) to self-trigger queue processing
    /// without waiting for a guest kick.
    ///
    /// The thread exits when the kick fd is dropped (epoll returns an error).
    fn spawn_kick_thread(&self, queue_idx: usize, kick_fd: File, extra_notifier: Option<File>) {
        let backend = Arc::clone(&self.backend);
        let vring_locks = Arc::clone(&self.vring_locks);
        thread::Builder::new()
            .name(format!("kick-q{queue_idx}"))
            .spawn(move || {
                use rustix::event::epoll;
                use std::os::unix::io::AsFd;

                log::debug!("kick-q{queue_idx}: thread started");
                let epoll_fd = match epoll::create(epoll::CreateFlags::CLOEXEC) {
                    Ok(fd) => fd,
                    Err(e) => {
                        log::error!("kick-q{queue_idx}: epoll create failed: {e}");
                        return;
                    }
                };
                // Register the guest kick eventfd (data=0).
                if let Err(e) = epoll::add(
                    &epoll_fd,
                    kick_fd.as_fd(),
                    epoll::EventData::new_u64(0),
                    epoll::EventFlags::IN,
                ) {
                    log::error!("kick-q{queue_idx}: epoll_add(kick) failed: {e}");
                    return;
                }
                // Optionally register the backend self-notify eventfd (data=1).
                if let Some(ref notify_fd) = extra_notifier {
                    if let Err(e) = epoll::add(
                        &epoll_fd,
                        notify_fd.as_fd(),
                        epoll::EventData::new_u64(1),
                        epoll::EventFlags::IN,
                    ) {
                        log::warn!("kick-q{queue_idx}: epoll_add(notify) failed: {e}");
                        // Continue without the extra notifier — not fatal.
                    } else {
                        log::debug!("kick-q{queue_idx}: extra notifier registered");
                    }
                }
                log::debug!("kick-q{queue_idx}: epoll ready, waiting for kicks");
                let mut events = [epoll::Event {
                    flags: epoll::EventFlags::empty(),
                    data: epoll::EventData::new_u64(0),
                }; 2];
                loop {
                    match epoll::wait(&epoll_fd, &mut events, None) {
                        Ok(0) => continue,
                        Ok(n) => {
                            // Drain all ready fds before calling handle_event once.
                            for event in &events[..n] {
                                let data = event.data.u64();
                                if data == 0 {
                                    log::debug!("kick-q{queue_idx}: got guest kick");
                                    let _ = rustix::io::read(&kick_fd, &mut [0u8; 8]);
                                } else {
                                    log::debug!("kick-q{queue_idx}: got backend notify");
                                    if let Some(ref nfd) = extra_notifier {
                                        let _ = rustix::io::read(nfd, &mut [0u8; 8]);
                                    }
                                }
                            }
                            // Collect configured vrings to pass to handle_event.
                            // VringRwLock is Clone (Arc<RwLock<_>>), so cloning is cheap.
                            let vrings: Vec<VringRwLock> = vring_locks
                                .lock()
                                .map(|g| g.iter().filter_map(Clone::clone).collect())
                                .unwrap_or_default();
                            if let Err(e) = backend.lock().unwrap().handle_event(
                                queue_idx as u16,
                                EventSet::IN,
                                &vrings,
                                0,
                            ) {
                                log::error!("kick-q{queue_idx}: handle_event error: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            log::debug!("kick-q{queue_idx}: epoll error (fd closed?): {e}");
                            break;
                        }
                    }
                }
            })
            .ok();
    }
}

impl<T: VhostUserBackendMut<Bitmap = (), Vring = VringRwLock> + QueueNotifier + Send + 'static>
    vhost::vhost_user::VhostUserBackendReqHandlerMut for BackendMutAdapter<T>
{
    fn set_owner(&mut self) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_owner");
        Ok(())
    }

    fn reset_owner(&mut self) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: reset_owner");
        Ok(())
    }

    fn reset_device(&mut self) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: reset_device");
        self.acked_features = 0;
        self.acked_protocol_features = 0;
        for v in &mut self.vrings {
            *v = VringState::default();
        }
        self.backend.lock().unwrap().reset_device();
        Ok(())
    }

    fn get_features(&mut self) -> VhostResult<u64> {
        let features = self.backend.lock().unwrap().features()
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        log::debug!("BackendMutAdapter: get_features -> {features:#x}");
        Ok(features)
    }

    fn set_features(&mut self, features: u64) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_features {features:#x}");
        self.acked_features = features;
        self.backend.lock().unwrap().acked_features(features);
        Ok(())
    }

    fn get_protocol_features(&mut self) -> VhostResult<VhostUserProtocolFeatures> {
        let proto = self.backend.lock().unwrap().protocol_features();
        log::debug!("BackendMutAdapter: get_protocol_features -> {proto:?}");
        Ok(proto)
    }

    fn set_protocol_features(&mut self, features: u64) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_protocol_features {features:#x}");
        self.acked_protocol_features = features;
        Ok(())
    }

    fn get_queue_num(&mut self) -> VhostResult<u64> {
        Ok(self.num_queues as u64)
    }

    fn set_mem_table(
        &mut self,
        ctx: &[VhostUserMemoryRegion],
        files: Vec<File>,
    ) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_mem_table ({} regions)", ctx.len());
        self.apply_mem_table(ctx, files);
        Ok(())
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_num idx={index} num={num}");
        if let Some(v) = self.vrings.get_mut(index as usize) {
            v.size = num;
        }
        Ok(())
    }

    fn set_vring_addr(
        &mut self,
        index: u32,
        _flags: VhostUserVringAddrFlags,
        descriptor: u64,
        used: u64,
        available: u64,
        _log: u64,
    ) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_addr idx={index} desc={descriptor:#x}");
        if let Some(v) = self.vrings.get_mut(index as usize) {
            v.desc_table = descriptor;
            v.used_ring = used;
            v.avail_ring = available;
        }
        Ok(())
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_base idx={index} base={base}");
        if let Some(v) = self.vrings.get_mut(index as usize) {
            v.base = base;
        }
        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> VhostResult<VhostUserVringState> {
        let base = self.vrings.get(index as usize).map_or(0, |v| v.base);
        log::debug!("BackendMutAdapter: get_vring_base idx={index} -> {base}");
        Ok(VhostUserVringState::new(index, base))
    }

    fn set_vring_kick(&mut self, index: u8, fd: Option<File>) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_kick idx={index}");
        if let Some(kick_fd) = fd {
            let queue_idx = usize::from(index);
            // Query the backend for an optional extra notifier fd for this queue.
            // This allows backends (e.g., virtio-console RX) to self-wake the kick
            // thread when data becomes available without waiting for a guest kick.
            let extra_notifier = self.backend.lock().unwrap().extra_kick_notifier(queue_idx);
            self.spawn_kick_thread(queue_idx, kick_fd, extra_notifier);
        }
        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, fd: Option<File>) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_call idx={index}");
        // Store the call fd; it will be passed to the VringRwLock in configure_vring()
        // so that signal_used_queue() writes to the KVM irqfd and injects MSI-X into the guest.
        if let Some(v) = self.vrings.get_mut(usize::from(index)) {
            v.call_fd = fd;
        }
        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, _fd: Option<File>) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_err idx={index}");
        Ok(())
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_vring_enable idx={index} enable={enable}");
        if let Some(v) = self.vrings.get_mut(index as usize) {
            v.enabled = enable;
        }
        // Configure the VringRwLock now that the queue is being enabled.
        // At this point set_vring_addr has been called (desc_table etc. are set)
        // and set_mem_table should have provided guest memory.
        if enable {
            self.configure_vring(index as usize);
        }
        Ok(())
    }

    fn get_config(
        &mut self,
        offset: u32,
        size: u32,
        _flags: VhostUserConfigFlags,
    ) -> VhostResult<Vec<u8>> {
        log::debug!("BackendMutAdapter: get_config offset={offset} size={size}");
        Ok(self.backend.lock().unwrap().get_config(offset, size))
    }

    fn set_config(
        &mut self,
        offset: u32,
        buf: &[u8],
        _flags: VhostUserConfigFlags,
    ) -> VhostResult<()> {
        log::debug!("BackendMutAdapter: set_config offset={offset}");
        self.backend
            .lock()
            .unwrap()
            .set_config(offset, buf)
            .map_err(|e| {
                vhost::vhost_user::Error::InvalidOperation(Box::leak(
                    e.to_string().into_boxed_str(),
                ))
            })
    }

    fn set_gpu_socket(&mut self, _gpu_backend: GpuBackend) -> VhostResult<()> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "GPU not supported",
        ))
    }

    fn get_shared_object(&mut self, _uuid: VhostUserSharedMsg) -> VhostResult<File> {
        Err(vhost::vhost_user::Error::InvalidOperation("not supported"))
    }

    fn get_inflight_fd(
        &mut self,
        _inflight: &VhostUserInflight,
    ) -> VhostResult<(VhostUserInflight, File)> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "inflight not supported",
        ))
    }

    fn set_inflight_fd(&mut self, _inflight: &VhostUserInflight, _file: File) -> VhostResult<()> {
        Ok(())
    }

    fn get_max_mem_slots(&mut self) -> VhostResult<u64> {
        Ok(509)
    }

    fn add_mem_region(
        &mut self,
        _region: &VhostUserSingleMemoryRegion,
        _fd: File,
    ) -> VhostResult<()> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "dynamic mem regions not supported",
        ))
    }

    fn remove_mem_region(&mut self, _region: &VhostUserSingleMemoryRegion) -> VhostResult<()> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "dynamic mem regions not supported",
        ))
    }

    fn set_device_state_fd(
        &mut self,
        _direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        _fd: File,
    ) -> VhostResult<Option<File>> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "device state transfer not supported",
        ))
    }

    fn check_device_state(&mut self) -> VhostResult<()> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "device state transfer not supported",
        ))
    }

    // Added in vhost 0.16's protocol-feature surface. Shared-memory
    // regions are a virtio-fs / virtio-gpu thing; we don't advertise
    // SHMEM_REGIONS in protocol_features() so this should never be
    // called, but the trait demands an implementation.
    fn get_shmem_config(
        &mut self,
    ) -> VhostResult<vhost::vhost_user::message::VhostUserShMemConfig> {
        Err(vhost::vhost_user::Error::InvalidOperation(
            "shared memory regions not supported",
        ))
    }

    fn set_log_base(
        &mut self,
        _log: &vhost::vhost_user::message::VhostUserLog,
        _file: File,
    ) -> VhostResult<()> {
        Ok(())
    }
}

/// Run a vhost-user backend loop over an inherited socket file descriptor.
///
/// Converts `fd` to a [`UnixStream`] and drives the vhost-user protocol request handler
/// in a loop until the frontend disconnects cleanly or an unrecoverable error occurs.
///
/// The backend `T` must implement [`VhostUserBackendMut`] with `Bitmap = ()` and
/// `Vring = VringRwLock`. The high-level trait is bridged to the low-level
/// [`BackendReqHandler`] via [`BackendMutAdapter`].
///
/// # Arguments
///
/// * `fd` - A valid, open, connected socket file descriptor inherited from the VMM parent
///   process. Ownership is transferred to this function; the caller must not use `fd`
///   again after this call.
/// * `backend` - The backend handler wrapped in `Arc<Mutex<T>>`.
///
/// # Returns
///
/// `Ok(())` on clean frontend disconnect. `Err(Error::Protocol(_))` on unrecoverable protocol errors.
#[allow(unsafe_code)]
pub fn run_backend<T>(fd: i32, backend: Arc<Mutex<T>>) -> Result<(), Error>
where
    T: VhostUserBackendMut<Bitmap = (), Vring = VringRwLock> + QueueNotifier + Send + 'static,
{
    // SAFETY: fd is a valid, open, connected socket inherited from the VMM parent process.
    // Ownership is transferred here; the fd is not used elsewhere after this call.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };

    let adapter = BackendMutAdapter::new(backend);
    let mut handler = BackendReqHandler::from_stream(socket, Arc::new(Mutex::new(adapter)));

    loop {
        match handler.handle_request() {
            Ok(()) => continue,
            Err(
                vhost::vhost_user::Error::Disconnected | vhost::vhost_user::Error::SocketBroken(_),
            ) => return Ok(()),
            Err(e) => return Err(Error::Protocol(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use vhost::vhost_user::Frontend;
    use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
    // Required to call set_owner, get_features, set_features on Frontend.
    use vhost::VhostBackend;
    use vhost_user_backend::{VhostUserBackendMut, VringRwLock};
    use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};
    use vmm_sys_util::epoll::EventSet;

    /// A minimal mock backend implementing VhostUserBackendMut for handshake testing.
    struct MockBackend {
        acked_features: u64,
    }

    impl MockBackend {
        fn new() -> Self {
            MockBackend { acked_features: 0 }
        }
    }

    impl QueueNotifier for MockBackend {
        // Use default: no extra notifiers.
    }

    /// VIRTIO_F_VERSION_1 (bit 32) combined with VHOST_USER_F_PROTOCOL_FEATURES (bit 30).
    const MOCK_FEATURES: u64 = (1u64 << 32) | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

    impl VhostUserBackendMut for MockBackend {
        type Bitmap = ();
        type Vring = VringRwLock;

        fn num_queues(&self) -> usize {
            1
        }

        fn max_queue_size(&self) -> usize {
            256
        }

        fn features(&self) -> u64 {
            MOCK_FEATURES
        }

        fn acked_features(&mut self, features: u64) {
            self.acked_features = features;
        }

        fn protocol_features(&self) -> VhostUserProtocolFeatures {
            VhostUserProtocolFeatures::empty()
        }

        fn set_event_idx(&mut self, _enabled: bool) {}

        fn update_memory(
            &mut self,
            _mem: GuestMemoryAtomic<GuestMemoryMmap>,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn handle_event(
            &mut self,
            _device_event: u16,
            _evset: EventSet,
            _vrings: &[VringRwLock],
            _thread_id: usize,
        ) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Verify a full vhost-user handshake (set_owner, get_features, set_features)
    /// works end-to-end over a socketpair using run_backend with VhostUserBackendMut.
    #[test]
    fn test_handshake_over_socketpair() {
        let (frontend_sock, backend_sock) = UnixStream::pair().unwrap();
        let backend = Arc::new(Mutex::new(MockBackend::new()));
        // Transfer ownership of the backend socket fd to run_backend.
        let backend_fd = backend_sock.into_raw_fd();

        let backend_thread = thread::spawn(move || run_backend(backend_fd, backend));

        let frontend = Frontend::from_stream(frontend_sock, 1);
        frontend.set_owner().unwrap();
        let features = frontend.get_features().unwrap();
        assert!(
            features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0,
            "PROTOCOL_FEATURES must be advertised by the mock backend"
        );
        // Set features back (clear bit 0 which is not supported as shown in vhost tests).
        frontend.set_features(features & !(1u64)).unwrap();

        // Drop frontend to close the socket — backend should exit cleanly.
        drop(frontend);

        let result = backend_thread.join().expect("backend thread panicked");
        assert!(result.is_ok(), "backend should exit cleanly on disconnect");
    }

    /// Verify that dropping the frontend immediately after set_owner returns Ok
    /// from run_backend, not a panic or protocol error.
    #[test]
    fn test_clean_disconnect() {
        let (frontend_sock, backend_sock) = UnixStream::pair().unwrap();
        let backend = Arc::new(Mutex::new(MockBackend::new()));
        let backend_fd = backend_sock.into_raw_fd();

        let backend_thread = thread::spawn(move || run_backend(backend_fd, backend));

        let frontend = Frontend::from_stream(frontend_sock, 1);
        frontend.set_owner().unwrap();

        // Immediately drop the frontend — simulates abrupt VMM exit.
        drop(frontend);

        let result = backend_thread.join().expect("backend thread panicked");
        assert!(
            result.is_ok(),
            "backend should exit cleanly on abrupt disconnect"
        );
    }
}
