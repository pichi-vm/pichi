//! Raw block-device I/O benchmarks for snuffler.
//!
//! For each whole-disk device, open `/dev/<name>` (preferring `O_DIRECT` so the
//! I/O actually traverses virtio-blk rather than the page cache) and measure
//! sequential + random read, and — on read-write devices — sequential + random
//! write with a read-back integrity check. Read-only devices are probed for
//! correct `O_RDWR`-open rejection instead.
//!
//! snuffler measures and reports; it asserts nothing. Throughput is telemetry
//! (useful for finding bottlenecks outside CI); only the correctness invariants
//! are stable enough for a harness to gate on. Writes are destructive — snuffler
//! is a disposable-VM probe; do not attach real data to a snuffler VM.

use snuffler::BlkBench;

#[cfg(target_os = "linux")]
pub(crate) fn benchmark_device(name: &str, size_bytes: u64, ro: bool) -> Option<BlkBench> {
    imp::benchmark_device(name, size_bytes, ro)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn benchmark_device(_name: &str, _size_bytes: u64, _ro: bool) -> Option<BlkBench> {
    None
}

#[cfg(target_os = "linux")]
mod imp {
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ffi::CString;
    use std::time::Instant;

    use snuffler::{BlkBench, BlkOp};

    /// Sequential transfer block size.
    const SEQ_BLK: usize = 64 * 1024;
    /// Random transfer block size (also the O_DIRECT alignment we target).
    const RAND_BLK: usize = 4096;
    /// Cap total sequential transfer so boot stays quick.
    const TOTAL_CAP: u64 = 16 * 1024 * 1024;
    /// Number of random ops (scaled down for tiny devices).
    const RAND_OPS: u64 = 256;

    pub(crate) fn benchmark_device(name: &str, size_bytes: u64, ro: bool) -> Option<BlkBench> {
        if size_bytes < RAND_BLK as u64 {
            return Some(err_bench(format!(
                "device too small to benchmark ({size_bytes} bytes)"
            )));
        }

        let Ok(path) = CString::new(format!("/dev/{name}")) else {
            return Some(err_bench("device path contains NUL".to_string()));
        };

        let want = if ro { libc::O_RDONLY } else { libc::O_RDWR };
        let (fd, mode) = open_direct(&path, want);
        if fd < 0 {
            return Some(err_bench(format!(
                "open /dev/{name}: errno {}",
                last_errno()
            )));
        }

        let total = round_down(size_bytes.min(TOTAL_CAP), RAND_BLK as u64);
        let seed = fnv1a(name.as_bytes());
        let mut buf = AlignedBuf::new(SEQ_BLK);

        let seq_read = seq_op(fd, total, &mut buf, IoDir::Read, 0);
        let rand_read = rand_op(fd, size_bytes, &mut buf, seed, IoDir::Read);

        let mut seq_write = None;
        let mut rand_write = None;
        let mut ro_write_rejected = None;

        if ro {
            // The reads above used an O_RDONLY fd. To prove the *device* (not the
            // fd mode) enforces read-only, try to open it O_RDWR: the kernel
            // rejects O_RDWR on a read-only gendisk with EROFS.
            // SAFETY: path is a valid NUL-terminated C string.
            let rw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if rw < 0 {
                ro_write_rejected = Some(true);
            } else {
                // Unexpected: opened a read-only device for write. Don't write;
                // just record the anomaly.
                // SAFETY: rw is a valid fd we own.
                unsafe { libc::close(rw) };
                ro_write_rejected = Some(false);
            }
        } else {
            seq_write = Some(seq_op(fd, total, &mut buf, IoDir::Write, seed));
            rand_write = Some(rand_op(
                fd,
                size_bytes,
                &mut buf,
                seed ^ 0xABCD_EF01,
                IoDir::Write,
            ));
        }

        // SAFETY: fd is a valid descriptor we opened.
        unsafe { libc::close(fd) };

        Some(BlkBench {
            mode: mode.to_string(),
            seq_read,
            rand_read,
            seq_write,
            rand_write,
            ro_write_rejected,
            error: None,
        })
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum IoDir {
        Read,
        Write,
    }

    /// Sequential read or write of `total` bytes from offset 0, in 64 KiB
    /// blocks. For writes, each block is filled with a per-offset pattern and a
    /// full read-back verifies it.
    fn seq_op(fd: i32, total: u64, buf: &mut AlignedBuf, dir: IoDir, seed: u64) -> BlkOp {
        let mut bytes = 0u64;
        let mut ops = 0u64;
        let mut errors = 0u64;
        let start = Instant::now();
        let mut off = 0u64;
        while off < total {
            let len = SEQ_BLK.min((total - off) as usize);
            let ok = match dir {
                IoDir::Read => unsafe { do_pread(fd, buf.ptr(), len, off) },
                IoDir::Write => {
                    fill_pattern(&mut buf.as_mut_slice()[..len], seed ^ off);
                    unsafe { do_pwrite(fd, buf.ptr(), len, off) }
                }
            };
            if ok {
                bytes += len as u64;
            } else {
                errors += 1;
            }
            ops += 1;
            off += len as u64;
        }
        let duration_us = start.elapsed().as_micros() as u64;

        let verified = (dir == IoDir::Write).then(|| {
            if errors != 0 {
                return false;
            }
            let vbuf = AlignedBuf::new(SEQ_BLK);
            let mut o = 0u64;
            while o < total {
                let len = SEQ_BLK.min((total - o) as usize);
                if !unsafe { do_pread(fd, vbuf.ptr(), len, o) } {
                    return false;
                }
                if !pattern_matches(&vbuf.as_slice()[..len], seed ^ o) {
                    return false;
                }
                o += len as u64;
            }
            true
        });

        BlkOp {
            bytes,
            ops,
            duration_us,
            throughput_mibps: mibps(bytes, duration_us),
            errors,
            verified,
        }
    }

    /// Random 4 KiB read or write at aligned offsets. For writes, each block is
    /// filled with a per-offset pattern and read back to verify.
    fn rand_op(fd: i32, size: u64, buf: &mut AlignedBuf, seed: u64, dir: IoDir) -> BlkOp {
        let nblocks = size / RAND_BLK as u64;
        if nblocks == 0 {
            return BlkOp {
                bytes: 0,
                ops: 0,
                duration_us: 0,
                throughput_mibps: 0.0,
                errors: 0,
                verified: (dir == IoDir::Write).then_some(true),
            };
        }
        let target = RAND_OPS.min(nblocks);
        let mut rng = Rng::new(seed);
        let mut offsets = Vec::with_capacity(target as usize);
        for _ in 0..target {
            offsets.push((rng.next_u64() % nblocks) * RAND_BLK as u64);
        }

        let mut bytes = 0u64;
        let mut errors = 0u64;
        let start = Instant::now();
        for &off in &offsets {
            let ok = match dir {
                IoDir::Read => unsafe { do_pread(fd, buf.ptr(), RAND_BLK, off) },
                IoDir::Write => {
                    fill_pattern(&mut buf.as_mut_slice()[..RAND_BLK], seed ^ off);
                    unsafe { do_pwrite(fd, buf.ptr(), RAND_BLK, off) }
                }
            };
            if ok {
                bytes += RAND_BLK as u64;
            } else {
                errors += 1;
            }
        }
        let duration_us = start.elapsed().as_micros() as u64;

        let verified = (dir == IoDir::Write).then(|| {
            if errors != 0 {
                return false;
            }
            let vbuf = AlignedBuf::new(RAND_BLK);
            for &off in &offsets {
                if !unsafe { do_pread(fd, vbuf.ptr(), RAND_BLK, off) } {
                    return false;
                }
                if !pattern_matches(vbuf.as_slice(), seed ^ off) {
                    return false;
                }
            }
            true
        });

        BlkOp {
            bytes,
            ops: offsets.len() as u64,
            duration_us,
            throughput_mibps: mibps(bytes, duration_us),
            errors,
            verified,
        }
    }

    fn err_bench(msg: String) -> BlkBench {
        let zero = BlkOp {
            bytes: 0,
            ops: 0,
            duration_us: 0,
            throughput_mibps: 0.0,
            errors: 0,
            verified: None,
        };
        BlkBench {
            mode: "none".to_string(),
            seq_read: zero.clone(),
            rand_read: zero,
            seq_write: None,
            rand_write: None,
            ro_write_rejected: None,
            error: Some(msg),
        }
    }

    /// Open `path` with `flags`, preferring `O_DIRECT`; fall back to buffered if
    /// the kernel/backing refuses direct I/O. Returns `(fd, mode)`.
    fn open_direct(path: &CString, flags: i32) -> (i32, &'static str) {
        // SAFETY: path is a valid NUL-terminated C string; open is total.
        let direct = unsafe { libc::open(path.as_ptr(), flags | libc::O_DIRECT | libc::O_CLOEXEC) };
        if direct >= 0 {
            return (direct, "o_direct");
        }
        // SAFETY: as above.
        let buffered = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
        (buffered, "buffered")
    }

    /// SAFETY: `ptr` must point to at least `len` writable bytes; `fd` valid.
    unsafe fn do_pread(fd: i32, ptr: *mut u8, len: usize, off: u64) -> bool {
        let n = unsafe { libc::pread(fd, ptr.cast(), len, off as libc::off_t) };
        n >= 0 && n as usize == len
    }

    /// SAFETY: `ptr` must point to at least `len` readable bytes; `fd` valid.
    unsafe fn do_pwrite(fd: i32, ptr: *const u8, len: usize, off: u64) -> bool {
        let n = unsafe { libc::pwrite(fd, ptr.cast(), len, off as libc::off_t) };
        n >= 0 && n as usize == len
    }

    fn last_errno() -> i32 {
        // SAFETY: __errno_location returns a valid thread-local int pointer.
        unsafe { *libc::__errno_location() }
    }

    fn mibps(bytes: u64, us: u64) -> f64 {
        if us == 0 {
            0.0
        } else {
            (bytes as f64 / 1_048_576.0) / (us as f64 / 1_000_000.0)
        }
    }

    fn round_down(x: u64, align: u64) -> u64 {
        x & !(align - 1)
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h | 1
    }

    /// Deterministic xorshift64 PRNG (reproducible random offsets, no deps).
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn fill_pattern(buf: &mut [u8], seed: u64) {
        let mut rng = Rng::new(seed);
        let mut i = 0;
        while i + 8 <= buf.len() {
            buf[i..i + 8].copy_from_slice(&rng.next_u64().to_le_bytes());
            i += 8;
        }
        if i < buf.len() {
            let tail = rng.next_u64().to_le_bytes();
            let rem = buf.len() - i;
            buf[i..].copy_from_slice(&tail[..rem]);
        }
    }

    fn pattern_matches(buf: &[u8], seed: u64) -> bool {
        let mut expect = vec![0u8; buf.len()];
        fill_pattern(&mut expect, seed);
        expect == buf
    }

    /// Heap buffer aligned to [`RAND_BLK`] for O_DIRECT I/O.
    struct AlignedBuf {
        ptr: *mut u8,
        len: usize,
        layout: Layout,
    }

    impl AlignedBuf {
        fn new(len: usize) -> Self {
            let layout = Layout::from_size_align(len.max(1), RAND_BLK).expect("aligned layout");
            // SAFETY: layout has non-zero size and power-of-two alignment.
            let ptr = unsafe { alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "aligned allocation failed");
            Self { ptr, len, layout }
        }
        fn ptr(&self) -> *mut u8 {
            self.ptr
        }
        fn as_slice(&self) -> &[u8] {
            // SAFETY: ptr/len describe a live allocation.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
        fn as_mut_slice(&mut self) -> &mut [u8] {
            // SAFETY: ptr/len describe a live, uniquely-borrowed allocation.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }
    }

    impl Drop for AlignedBuf {
        fn drop(&mut self) {
            // SAFETY: ptr came from alloc_zeroed with this exact layout.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }
}
