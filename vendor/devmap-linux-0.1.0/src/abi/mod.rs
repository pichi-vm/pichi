// SPDX-License-Identifier: Apache-2.0

//! Private device-mapper ioctl representation.

mod command;
mod header;
mod spec;

pub(crate) use command::Command;
pub(crate) use header::Header;
pub(crate) use spec::TargetSpec;
