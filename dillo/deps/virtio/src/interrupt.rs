// SPDX-License-Identifier: Apache-2.0

//! Cross-platform guest **interrupt** signal (the virtio "call" path).
//!
//! After a device worker consumes descriptors it signals the guest that the
//! used ring advanced. On Linux this is an irqfd `eventfd` — writing it makes
//! KVM inject the device's MSI-X. On macOS/HVF there is no irqfd: the VMM
//! supplies a closure that calls `hv_gic_send_msi(addr, intid)` with the
//! values from the device's MSI-X table entry. Keeping the macOS variant a
//! closure keeps this crate free of any hypervisor binding.

pub use imp::Interrupt;

#[cfg(target_os = "linux")]
mod imp {
    use vmm_sys_util::eventfd::EventFd;

    /// Linux interrupt: an irqfd eventfd. `signal` writes it; KVM injects MSI-X.
    #[derive(Debug)]
    pub struct Interrupt(EventFd);

    impl Interrupt {
        pub fn from_eventfd(fd: EventFd) -> Self {
            Self(fd)
        }

        pub fn signal(&self) -> std::io::Result<()> {
            self.0.write(1)
        }

        pub fn try_clone(&self) -> std::io::Result<Self> {
            Ok(Self(self.0.try_clone()?))
        }

        /// Underlying eventfd, for vhost-user `set_vring_call` (Linux only).
        pub fn as_eventfd(&self) -> &EventFd {
            &self.0
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::sync::Arc;

    /// macOS/HVF interrupt: a closure supplied by the VMM that raises the
    /// guest interrupt (typically `hv_gic_send_msi(addr, intid)`).
    #[derive(Clone)]
    pub struct Interrupt(Arc<dyn Fn() + Send + Sync>);

    impl Interrupt {
        pub fn from_fn(raise: impl Fn() + Send + Sync + 'static) -> Self {
            Self(Arc::new(raise))
        }

        pub fn signal(&self) -> std::io::Result<()> {
            (self.0)();
            Ok(())
        }

        pub fn try_clone(&self) -> std::io::Result<Self> {
            Ok(self.clone())
        }
    }

    impl std::fmt::Debug for Interrupt {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Interrupt").finish_non_exhaustive()
        }
    }
}
