// SPDX-License-Identifier: Apache-2.0

//! Library surface for the `pichi` binary crate.
//!
//! Exists so integration tests under `tests/` (which form a separate crate)
//! can call internal helpers and command modules that are not otherwise
//! reachable through the binary CLI.
//!
//! The binary entry point lives in `src/main.rs`; `main` is intentionally
//! NOT exported here.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cli;
pub mod cmd;
pub mod config;
pub mod system;
