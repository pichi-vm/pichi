//! virtio-console — both the in-process [`VirtioDevice`] (thread-only
//! build) and the cross-process [`run_backend`] entrypoint (default
//! process-isolation build, invoked via `dillo backend console
//! --fd=N` after fork+exec).
//!
//! Thread-mode: the device is driven directly by the VM process. The
//! transport (virtio-pci, in crate `virtio-pci`) wraps this device,
//! handles config-space + BAR MMIO, and calls [`activate`] once the
//! guest writes DRIVER_OK. We then spawn a TX worker that drains the
//! TX queue and writes the bytes to stdout, plus an RX worker that
//! forwards host stdin into guest-provided receive buffers.
//!
//! Two queues per virtio-console spec §5.3:
//! - Queue 0: RX (host → guest input from stdin).
//! - Queue 1: TX (guest → host output → stdout).
//!
//! No multiport / control queue support (we don't negotiate
//! VIRTIO_CONSOLE_F_MULTIPORT). Single-console only — that's what
//! `console=hvc0` needs.
//!
//! See `dillo/ARCHITECTURE.md` §10 and §12.

use std::collections::VecDeque;
use std::io::{self, BufWriter, Read, Write};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::thread;

use virtio::queue::{Queue, VIRTQ_DESC_F_WRITE};
use virtio::{ActivateError, Interrupt, Kick, VirtioDevice};
use vm_memory::{Bytes, GuestMemoryMmap};

/// VIRTIO_F_VERSION_1 from the virtio 1.x spec.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Virtio device-type id for a console (virtio 1.x §5.3).
pub const VIRTIO_ID_CONSOLE: u32 = 3;

/// Queue sizes (per spec a power of two; 64 is the typical hvc0 size).
const QUEUE_MAX: u16 = 64;
const QUEUE_SIZES: [u16; 2] = [QUEUE_MAX, QUEUE_MAX];

enum OutputMessage {
    Data(Vec<u8>),
    Flush(mpsc::Sender<()>),
}

fn output_tx() -> &'static Mutex<mpsc::Sender<OutputMessage>> {
    static OUTPUT_TX: OnceLock<Mutex<mpsc::Sender<OutputMessage>>> = OnceLock::new();
    OUTPUT_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("virtio-console-stdout".into())
            .spawn(move || output_worker(rx))
            .expect("spawn virtio-console stdout worker");
        Mutex::new(tx)
    })
}

fn output_worker(rx: mpsc::Receiver<OutputMessage>) {
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    while let Ok(msg) = rx.recv() {
        match msg {
            OutputMessage::Data(output) => {
                let _ = stdout.write_all(&output);
            }
            OutputMessage::Flush(done) => {
                let _ = stdout.flush();
                let _ = done.send(());
            }
        }
    }
}

fn enqueue_output(output: Vec<u8>) {
    if output.is_empty() {
        return;
    }
    if let Ok(tx) = output_tx().lock() {
        let _ = tx.send(OutputMessage::Data(output));
    }
}

pub fn flush_output() {
    let (done_tx, done_rx) = mpsc::channel();
    if let Ok(tx) = output_tx().lock() {
        let _ = tx.send(OutputMessage::Flush(done_tx));
    }
    let _ = done_rx.recv();
}

/// Resolve the guest [`Interrupt`] for a given MSI-X vector at activate time.
/// On Linux the dillo-vm side returns an irqfd-backed interrupt; on macOS/HVF
/// it returns a closure that calls `hv_gic_send_msi` with the MSI-X entry.
pub type CallFdLookup = Arc<dyn Fn(u16) -> Option<Interrupt> + Send + Sync>;

/// virtio-console: thread-mode device.
pub struct VirtioConsole {
    call_fd_lookup: CallFdLookup,
    activated: bool,
}

impl std::fmt::Debug for VirtioConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioConsole")
            .field("activated", &self.activated)
            .finish()
    }
}

impl VirtioConsole {
    pub fn new(call_fd_lookup: CallFdLookup) -> Self {
        Self {
            call_fd_lookup,
            activated: false,
        }
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_CONSOLE
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        // Only VIRTIO_F_VERSION_1. No multiport, no size, no emerg-write.
        // Linux's hvc_virtio binds hvc0 with this minimal feature set.
        VIRTIO_F_VERSION_1
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        mut queues: Vec<Queue>,
        mut queue_evts: Vec<Kick>,
    ) -> Result<(), ActivateError> {
        if self.activated {
            return Err(ActivateError::InvalidConfig(
                "VirtioConsole::activate called twice".into(),
            ));
        }
        if queues.len() != 2 || queue_evts.len() != 2 {
            return Err(ActivateError::InvalidConfig(format!(
                "expected 2 queues + 2 evts, got {} / {}",
                queues.len(),
                queue_evts.len()
            )));
        }

        // Queue 1 is TX. Pop it out and spawn a worker.
        let tx_queue = queues.remove(1);
        let tx_evt = queue_evts.remove(1);
        let tx_call_fd = (self.call_fd_lookup)(tx_queue.msix_vector);
        spawn_tx_worker(mem.clone(), tx_queue, tx_evt, tx_call_fd);

        // Queue 0 is RX. Feed host stdin into guest-provided writable buffers.
        let rx_queue = queues.remove(0);
        let rx_evt = queue_evts.remove(0);
        let rx_call_fd = (self.call_fd_lookup)(rx_queue.msix_vector);
        spawn_rx_worker(mem.clone(), rx_queue, rx_evt, rx_call_fd);

        self.activated = true;
        log::info!("virtio-console: activated (TX/RX workers spawned)");
        Ok(())
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        // No fields advertised (no multiport, no max_nr_ports). Spec
        // says the device-config region size is 12 bytes, all zero
        // when no relevant features are negotiated.
        for b in data.iter_mut() {
            *b = 0;
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // No writable fields.
    }
}

fn spawn_tx_worker(mem: GuestMemoryMmap, queue: Queue, kick: Kick, call_fd: Option<Interrupt>) {
    thread::Builder::new()
        .name("virtio-console-tx".into())
        .spawn(move || tx_worker(mem, queue, kick, call_fd))
        .expect("spawn virtio-console TX worker");
}

fn spawn_rx_worker(mem: GuestMemoryMmap, queue: Queue, kick: Kick, call_fd: Option<Interrupt>) {
    let (input_tx, input_rx) = mpsc::channel();
    thread::Builder::new()
        .name("virtio-console-stdin".into())
        .spawn(move || stdin_worker(input_tx))
        .expect("spawn virtio-console stdin worker");
    thread::Builder::new()
        .name("virtio-console-rx".into())
        .spawn(move || rx_worker(mem, queue, kick, call_fd, input_rx))
        .expect("spawn virtio-console RX worker");
}

fn stdin_worker(tx: mpsc::Sender<Vec<u8>>) {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("virtio-console stdin: read error: {e}");
                return;
            }
        }
    }
}

fn tx_worker(mem: GuestMemoryMmap, queue: Queue, kick: Kick, call_fd: Option<Interrupt>) {
    // Linux: virtio-pci's queue eventfds are created `EFD_NONBLOCK` (so the
    // ioeventfd-side write never blocks). For the worker we want blocking
    // reads — clear O_NONBLOCK so each kick.read() suspends the worker until
    // KVM injects the next queue notify. On macOS the Kick is a condvar that
    // blocks natively, so there is no fd to adjust.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: fcntl is a pure syscall; no aliasing concerns. EAGAIN from
        // F_GETFL/F_SETFL is the only failure mode and we just log.
        #[allow(unsafe_code)]
        unsafe {
            let fd = kick.as_eventfd().as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
    }

    let queue = Arc::new(Mutex::new(queue));
    loop {
        if let Err(e) = kick.read() {
            log::error!("virtio-console TX: kick eventfd read error: {e}");
            return;
        }
        drain_tx(&mem, &queue, call_fd.as_ref());
    }
}

fn rx_worker(
    mem: GuestMemoryMmap,
    queue: Queue,
    kick: Kick,
    call_fd: Option<Interrupt>,
    input_rx: mpsc::Receiver<Vec<u8>>,
) {
    #[cfg(target_os = "linux")]
    clear_kick_nonblock(&kick);

    let queue = Arc::new(Mutex::new(queue));
    let mut pending = VecDeque::new();
    loop {
        if pending.is_empty() {
            match input_rx.recv() {
                Ok(bytes) => pending.extend(bytes),
                Err(_) => return,
            }
        }

        while !pending.is_empty() {
            if drain_rx(&mem, &queue, &mut pending, call_fd.as_ref()) {
                continue;
            }
            if let Err(e) = kick.read() {
                log::error!("virtio-console RX: kick eventfd read error: {e}");
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn clear_kick_nonblock(kick: &Kick) {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl is a pure syscall; failures are non-fatal and only affect
    // whether the device worker spins or blocks.
    #[allow(unsafe_code)]
    unsafe {
        let fd = kick.as_eventfd().as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

fn drain_rx(
    mem: &GuestMemoryMmap,
    queue: &Arc<Mutex<Queue>>,
    pending: &mut VecDeque<u8>,
    call_fd: Option<&Interrupt>,
) -> bool {
    let mut q = queue.lock().expect("virtio-console RX queue mutex");
    let mut signaled = false;
    let mut made_progress = false;
    while !pending.is_empty() {
        let Some(head) = q.pop(mem) else {
            break;
        };
        let head_index = head.index;
        let mut written: u32 = 0;
        let mut current = Some(head);
        while let Some(desc) = current {
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                let n = pending.len().min(desc.len as usize);
                if n != 0 {
                    let chunk: Vec<u8> = pending.drain(..n).collect();
                    match mem.write(&chunk, desc.addr) {
                        Ok(bytes) => {
                            written = written.saturating_add(bytes as u32);
                            made_progress = true;
                            if bytes < chunk.len() {
                                for byte in chunk[bytes..].iter().rev() {
                                    pending.push_front(*byte);
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "virtio-console RX: guest write at {:#x}+{}: {e:?}",
                                desc.addr.0,
                                desc.len
                            );
                            for byte in chunk.iter().rev() {
                                pending.push_front(*byte);
                            }
                            break;
                        }
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            current = desc.next_desc(mem);
        }
        q.add_used(mem, head_index, written);
        signaled = true;
    }
    if signaled {
        if let Some(intr) = call_fd {
            if let Err(e) = intr.signal() {
                log::warn!("virtio-console RX: signal interrupt: {e}");
            }
        }
    }
    made_progress
}

fn drain_tx(mem: &GuestMemoryMmap, queue: &Arc<Mutex<Queue>>, call_fd: Option<&Interrupt>) {
    let mut q = queue.lock().expect("virtio-console TX queue mutex");
    let mut signaled = false;
    let mut output = Vec::new();
    while let Some(head) = q.pop(mem) {
        let head_index = head.index;
        let mut written: u32 = 0;
        // Walk the chain manually — DescriptorChain isn't an Iterator,
        // it carries a `next_desc(mem)` accessor.
        let mut current = Some(head);
        while let Some(desc) = current {
            // Device-readable = guest-to-host data (TX path).
            if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                let mut buf = vec![0u8; desc.len as usize];
                match mem.read(&mut buf, desc.addr) {
                    Ok(n) => {
                        output.extend_from_slice(&buf[..n]);
                        written += n as u32;
                    }
                    Err(e) => {
                        log::warn!(
                            "virtio-console TX: guest read at {:#x}+{}: {e:?}",
                            desc.addr.0,
                            desc.len
                        );
                    }
                }
            }
            current = desc.next_desc(mem);
        }
        q.add_used(mem, head_index, written);
        signaled = true;
    }
    if signaled {
        enqueue_output(output);
        if let Some(intr) = call_fd {
            // Tell the guest one or more descriptors completed.
            if let Err(e) = intr.signal() {
                log::warn!("virtio-console TX: signal interrupt: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio::queue::VIRTQ_DESC_F_WRITE;
    use vm_memory::{Address, GuestAddress};

    #[test]
    fn rx_drains_pending_input_into_guest_buffer() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut queue = Queue::new(16);
        queue.size = 16;
        queue.ready = true;
        queue.desc_table = GuestAddress(0x100);
        queue.avail_ring = GuestAddress(0x1000);
        queue.used_ring = GuestAddress(0x2000);

        mem.write_obj::<u64>(0x5000, queue.desc_table).unwrap();
        mem.write_obj::<u32>(8, queue.desc_table.unchecked_add(8))
            .unwrap();
        mem.write_obj::<u16>(VIRTQ_DESC_F_WRITE, queue.desc_table.unchecked_add(12))
            .unwrap();
        mem.write_obj::<u16>(0, queue.desc_table.unchecked_add(14))
            .unwrap();
        mem.write_obj::<u16>(0, queue.avail_ring.unchecked_add(4))
            .unwrap();
        mem.write_obj::<u16>(1, queue.avail_ring.unchecked_add(2))
            .unwrap();

        let queue = Arc::new(Mutex::new(queue));
        let mut pending: VecDeque<u8> = b"abc".iter().copied().collect();
        assert!(drain_rx(&mem, &queue, &mut pending, None));
        assert!(pending.is_empty());

        let mut out = [0u8; 3];
        mem.read(&mut out, GuestAddress(0x5000)).unwrap();
        assert_eq!(&out, b"abc");
        let used_idx: u16 = mem.read_obj(GuestAddress(0x2002)).unwrap();
        assert_eq!(used_idx, 1);
    }
}

// ─── Process-mode backend entrypoint (ARCH §4.1 device child) ───

/// Post-fork+exec entrypoint for `dillo backend console --fd=N`.
///
/// Owns the inherited vhost-user socketpair half, installs seccomp
/// + PR_SET_PDEATHSIG, runs the vhost-user-backend handler loop via
/// the `vhost-backend` crate's `run_backend` adapter, and exits 0
/// on clean disconnect.
///
/// Implements [`vhost_user_backend::VhostUserBackendMut`] +
/// [`vhost_backend::QueueNotifier`] for a `ConsoleBackend`. Most
/// trait methods take defaults; the real work happens in
/// `handle_event` (one of our worker threads or queue-kick path).
#[cfg(target_os = "linux")]
pub fn run_backend(fd: i32) -> i32 {
    use std::sync::{Arc, Mutex};

    // PR_SET_PDEATHSIG SIGTERM: if the supervisor dies, the kernel
    // sends us SIGTERM so we don't outlive it (zombie backend).
    // SAFETY: prctl is a pure syscall; pid_t arg is meaningful only
    // for some sub-options, not ours.
    #[allow(unsafe_code)]
    unsafe {
        let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
    }
    // Race guard per the PoC: if the parent already died, PPID is 1.
    // SAFETY: getppid is a pure syscall.
    #[allow(unsafe_code)]
    let ppid = unsafe { libc::getppid() };
    if ppid == 1 {
        eprintln!("dillo backend console: orphaned at startup; exiting");
        return 1;
    }

    // Install seccomp filter. KillProcess on violation matches ARCH
    // §4.3 / §13.4 code 148 convention. extrasafe handles building
    // a tight allowlist for the vhost-user loop's syscall surface.
    install_console_seccomp();

    let backend = ConsoleBackend::new();
    let backend_arc = Arc::new(Mutex::new(backend));
    match vhost_backend::run_backend(fd, backend_arc) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dillo backend console: {e}");
            1
        }
    }
}

/// Stub on non-Linux hosts — process isolation is Linux-only.
#[cfg(not(target_os = "linux"))]
pub fn run_backend(_fd: i32) -> i32 {
    eprintln!("dillo backend console: process-isolation is Linux-only");
    1
}

#[cfg(target_os = "linux")]
fn install_console_seccomp() {
    // Build a filter tight enough to be a §4.3 boundary, wide enough
    // for the vhost-user handshake + the data plane:
    //   - BasicCapabilities: mmap/munmap/futex/sigaltstack/exit/...
    //   - SystemIO: stdout + stderr (TX-drain writes), close + ioctl
    //     (vhost-user-backend frees fds + drives EFD_NONBLOCK fcntls),
    //     read/write (memfd-mapped region reads in handle_event).
    //   - Networking::allow_running_unix_clients: recvmsg/sendmsg on
    //     the inherited socketpair, plus epoll + eventfd for the
    //     handler-thread event loop.
    // KillProcess on anything else.
    use extrasafe::builtins::{danger_zone::Threads, BasicCapabilities, Networking, SystemIO};
    use extrasafe::SafetyContext;
    let ctx = SafetyContext::new()
        .enable(BasicCapabilities)
        .and_then(|c| c.enable(Threads::nothing().allow_create()))
        .and_then(|c| {
            // allow_read/write override the per-fd stdout/stderr rules,
            // so we use them directly instead of stacking conflicting
            // conditionals. The child has stdin/stdout/stderr +
            // socket + memfd fds open; broader fd scope is acceptable
            // given the seccomp boundary already excludes everything
            // outside this ruleset.
            c.enable(
                SystemIO::nothing()
                    .allow_close()
                    .allow_ioctl()
                    .allow_read()
                    .allow_write(),
            )
        })
        .and_then(|c| c.enable(Networking::nothing().allow_running_unix_clients()));
    match ctx {
        Ok(c) => {
            if let Err(e) = c.apply_to_all_threads() {
                log::warn!("console seccomp install failed: {e}");
            } else {
                log::info!("console seccomp filter installed");
            }
        }
        Err(e) => log::warn!("console seccomp build failed: {e}"),
    }
}

// ─── ConsoleBackend: vhost-user trait impl ──────────────────────

#[cfg(target_os = "linux")]
mod backend {
    use std::io::Write;
    use std::ops::Deref;

    use vhost_user_backend::{VhostUserBackendMut, VringRwLock, VringStateMutGuard, VringT};
    use vm_memory::{Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
    use vmm_sys_util::epoll::EventSet;

    use vhost::vhost_user::message::VhostUserProtocolFeatures;
    use vhost_backend::QueueNotifier;
    use virtio_queue::QueueOwnedT;

    use super::VIRTIO_F_VERSION_1;

    /// vhost-user-side console backend. Drives the TX queue (idx 1)
    /// by draining each kicked descriptor chain to stdout, then
    /// signaling the frontend via the call eventfd.
    ///
    /// RX (idx 0) is parked — the guest may post buffers, but we
    /// never fill them. Phase 4 will plumb host stdin through here.
    pub struct ConsoleBackend {
        mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
        event_idx: bool,
    }

    impl std::fmt::Debug for ConsoleBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ConsoleBackend")
                .field("event_idx", &self.event_idx)
                .finish_non_exhaustive()
        }
    }

    impl Default for ConsoleBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ConsoleBackend {
        pub fn new() -> Self {
            Self {
                mem: None,
                event_idx: false,
            }
        }
    }

    impl QueueNotifier for ConsoleBackend {
        fn extra_kick_notifier(&self, _queue_idx: usize) -> Option<std::fs::File> {
            None
        }
    }

    impl VhostUserBackendMut for ConsoleBackend {
        type Bitmap = ();
        type Vring = VringRwLock;

        fn num_queues(&self) -> usize {
            2
        }
        fn max_queue_size(&self) -> usize {
            64
        }
        fn features(&self) -> u64 {
            VIRTIO_F_VERSION_1
        }
        fn protocol_features(&self) -> VhostUserProtocolFeatures {
            VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::CONFIG
        }
        fn set_event_idx(&mut self, enabled: bool) {
            self.event_idx = enabled;
        }
        fn update_memory(
            &mut self,
            mem: GuestMemoryAtomic<GuestMemoryMmap>,
        ) -> std::io::Result<()> {
            self.mem = Some(mem);
            Ok(())
        }
        fn get_config(&self, _offset: u32, _size: u32) -> Vec<u8> {
            vec![0; 12]
        }
        fn handle_event(
            &mut self,
            device_event: u16,
            _evset: EventSet,
            vrings: &[Self::Vring],
            _thread_id: usize,
        ) -> std::io::Result<()> {
            // Queue index 1 = TX. Per virtio 1.x §5.3 the guest writes
            // bytes into the TX queue's descriptor chains; we read
            // them and emit to stdout.
            if device_event != 1 {
                return Ok(());
            }
            let Some(mem_atomic) = self.mem.as_ref() else {
                return Ok(());
            };
            let Some(vring) = vrings.get(1) else {
                return Ok(());
            };

            let mem_guard = mem_atomic.memory();
            let mem: &GuestMemoryMmap = mem_guard.deref();

            // Collect completions outside the iter borrow on Queue so
            // we can call vring_state.add_used() afterwards (which also
            // borrows the queue mutably via VringState).
            let mut completions: Vec<(u16, u32)> = Vec::new();
            let mut stdout = std::io::stdout().lock();
            {
                let mut vstate: <Self::Vring as VringStateMutGuard<'_, _>>::G = vring.get_mut();
                let queue = vstate.get_queue_mut();
                let avail_iter = match queue.iter(mem) {
                    Ok(it) => it,
                    Err(e) => {
                        log::warn!("console backend: queue iter: {e}");
                        return Ok(());
                    }
                };
                for chain in avail_iter {
                    let head = chain.head_index();
                    let mut written: u32 = 0;
                    for desc in chain.readable() {
                        let len = desc.len() as usize;
                        if len == 0 {
                            continue;
                        }
                        let mut buf = vec![0u8; len];
                        match mem.read(&mut buf, desc.addr()) {
                            Ok(n) => {
                                let _ = stdout.write_all(&buf[..n]);
                                written += n as u32;
                            }
                            Err(e) => {
                                log::warn!(
                                    "console backend: guest read at {:#x}+{}: {e:?}",
                                    desc.addr().0,
                                    desc.len()
                                );
                            }
                        }
                    }
                    completions.push((head, written));
                }
            }
            let _ = stdout.flush();
            drop(stdout);

            if !completions.is_empty() {
                let mut vstate = vring.get_mut();
                for (head, written) in completions {
                    if let Err(e) = vstate.add_used(head, written) {
                        log::warn!("console backend: add_used: {e}");
                    }
                }
                if let Err(e) = vstate.signal_used_queue() {
                    log::warn!("console backend: signal_used_queue: {e}");
                }
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub use backend::ConsoleBackend;
