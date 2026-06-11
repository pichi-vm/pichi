//! carapace assembler CLI — read-side only.
//!
//! A thin command-line wrapper over the `carapace` library (see `lib.rs` /
//! `SPEC.md`): it parses the `attach` / `detach` verbs and delegates to
//! [`carapace::attach`] / [`carapace::detach`]. All chain-walk, validation, and
//! dm-stack logic lives in the library so in-process consumers can reuse it.

#![deny(unsafe_code)]
#![cfg(target_os = "linux")]

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
