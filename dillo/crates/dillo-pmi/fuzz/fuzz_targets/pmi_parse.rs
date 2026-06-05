//! Fuzz target: `dillo_pmi::parse(bytes, opts)`.
//!
//! Run with `cargo +nightly fuzz run pmi_parse` from this crate.
//! Asserts that the parser never panics, never OOMs (resource caps
//! enforce bounds), and that any rejection produces a typed
//! `dillo_pmi::Error` rather than aborting.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let opts = dillo_pmi::ParseOptions {
        host_arch: dillo_pmi::HostArch::X86_64,
        memory_mib: 4096,
    };
    let _ = dillo_pmi::parse(data, &opts);
});
