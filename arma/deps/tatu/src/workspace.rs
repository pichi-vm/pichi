//! Bare-metal workspace primitive.
//!
//! [`PageCell`] is the shared interior-mutability wrapper for every
//! section static; the statics themselves (stack, DTB buffers, boot CPU
//! tables, ACPI workspace, pads) live in [`crate::sections`] and the
//! per-arch modules. Per ARCHITECTURE.md §6.3, addresses come from those
//! statics (`&STATIC as *const _ as u64`), not free-standing linker
//! symbols. Unsafe is module-local with `// SAFETY:` notes per block
//! (§7.1).

#![allow(unsafe_code)]

use core::cell::UnsafeCell;

/// Container for a static workspace. `UnsafeCell` lets us obtain a
/// `*mut T` from a `&'static Self` and tells the compiler the
/// contents may change underneath it.
#[repr(transparent)]
pub struct PageCell<T>(UnsafeCell<T>);

// SAFETY: tatu runs on a single boot vCPU with interrupts disabled
// and no APs brought up. There is no concurrent access to any
// workspace; `Sync` is vacuously satisfied.
unsafe impl<T> Sync for PageCell<T> {}

impl<T> PageCell<T> {
    pub const fn new(t: T) -> Self {
        Self(UnsafeCell::new(t))
    }
    pub fn as_mut_ptr(&self) -> *mut T {
        self.0.get()
    }
}
