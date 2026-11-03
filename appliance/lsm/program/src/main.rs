// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: GPL-2.0-only OR BSD-2-Clause

#![no_std]
#![no_main]

use aya_ebpf::{helpers::bpf_probe_read_kernel_str_bytes, macros::lsm, programs::LsmContext};

#[lsm(hook = "inode_xattr_skipcap")]
pub fn skip_label_cap(context: LsmContext) -> i32 {
    let previous: i32 = context.arg(1);
    if previous != 0 {
        return previous;
    }
    let name: *const u8 = context.arg(0);
    // One byte beyond the expected name and its terminator distinguishes a
    // longer name even when the helper truncates its output.
    let mut buffer = [0; 18];
    // SAFETY: this hook supplies a kernel string pointer. The BPF helper
    // bounds the read to buffer and reports inaccessible memory as an error.
    let name = unsafe { bpf_probe_read_kernel_str_bytes(name, &mut buffer) };
    i32::from(name.ok() == Some(b"security.selinux"))
}

#[unsafe(link_section = "license")]
#[used]
static LICENSE: [u8; 13] = *b"Dual BSD/GPL\0";

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
