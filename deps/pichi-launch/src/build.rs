// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Command, Input};
use serde::{Deserialize, Serialize};

/// Null body of command `"build"`, version 1; its response payload is [`Input`].
/// Construct with [`Build::default`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Build(Option<()>);

impl Command for Build {
    const NAME: &'static str = "build";
    const VERSION: u16 = 1;

    type Response = Input;
}
