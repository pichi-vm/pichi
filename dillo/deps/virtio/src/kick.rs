// SPDX-License-Identifier: Apache-2.0

//! Cross-platform virtqueue **kick** primitive.
//!
//! A kick signals "this queue has new descriptors". On Linux a kick is an
//! `eventfd`: KVM's ioeventfd raises it in-kernel on the guest's notify-write,
//! and the device worker blocks on it. macOS/HVF has no ioeventfd, so the kick
//! is an in-process condvar counter that the MMIO notify path raises directly
//! (see `virtio-pci`'s `notify_write`).
//!
//! The API mirrors the subset of `EventFd` that the device workers use
//! (`read`/`write`/`try_clone`). On Linux, `as_eventfd()` exposes the
//! underlying `EventFd` for KVM ioeventfd registration; it does not exist on
//! other platforms.

pub use imp::Kick;

impl dillo_mmio::MmioNotifyEvent for Kick {
    #[cfg(target_os = "linux")]
    fn as_eventfd(&self) -> &vmm_sys_util::eventfd::EventFd {
        Kick::as_eventfd(self)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use vmm_sys_util::eventfd::EventFd;

    /// Linux kick: a thin wrapper over a non-blocking, close-on-exec eventfd.
    #[derive(Debug)]
    pub struct Kick(EventFd);

    impl Kick {
        pub fn new() -> std::io::Result<Self> {
            Ok(Self(EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?))
        }

        pub fn read(&self) -> std::io::Result<u64> {
            self.0.read()
        }

        pub fn write(&self, count: u64) -> std::io::Result<()> {
            self.0.write(count)
        }

        pub fn try_clone(&self) -> std::io::Result<Self> {
            Ok(Self(self.0.try_clone()?))
        }

        /// Underlying eventfd, for KVM ioeventfd registration (Linux only).
        pub fn as_eventfd(&self) -> &EventFd {
            &self.0
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::sync::{Arc, Condvar, Mutex};

    /// macOS/HVF kick: an in-process counting condvar. `read` blocks until at
    /// least one kick is pending, then drains the accumulated count; `write`
    /// adds to the count and wakes a waiter. Clones share one counter.
    #[derive(Clone, Debug)]
    pub struct Kick(Arc<Inner>);

    #[derive(Debug)]
    struct Inner {
        count: Mutex<u64>,
        cv: Condvar,
    }

    impl Kick {
        pub fn new() -> std::io::Result<Self> {
            Ok(Self(Arc::new(Inner {
                count: Mutex::new(0),
                cv: Condvar::new(),
            })))
        }

        pub fn read(&self) -> std::io::Result<u64> {
            let mut count = self.0.count.lock().expect("kick mutex poisoned");
            while *count == 0 {
                count = self.0.cv.wait(count).expect("kick mutex poisoned");
            }
            Ok(std::mem::take(&mut *count))
        }

        pub fn write(&self, count: u64) -> std::io::Result<()> {
            let mut guard = self.0.count.lock().expect("kick mutex poisoned");
            *guard = guard.saturating_add(count);
            self.0.cv.notify_one();
            Ok(())
        }

        pub fn try_clone(&self) -> std::io::Result<Self> {
            Ok(Self(Arc::clone(&self.0)))
        }
    }
}
