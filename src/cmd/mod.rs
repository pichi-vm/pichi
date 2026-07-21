// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Local cache management commands (Phase 42 / LOCAL-01..05) and image
//! import (Phase 43 / IMPORT-01..07).
//!
//! Each submodule exposes `pub fn run(args, &Config) -> anyhow::Result<()>`
//! mirroring the pattern set by `crate::system::run`.

pub mod build;
pub mod images;
pub mod import;
pub mod inspect;
pub mod prune;
pub mod pull;
pub mod push;
pub mod registry_helpers;
pub mod requirements;
pub mod rmi;
pub mod run;
pub mod streaming_sink;
pub mod tag;
pub mod update;
