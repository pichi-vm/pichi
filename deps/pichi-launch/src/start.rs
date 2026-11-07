// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Command, Input};
use serde::{Deserialize, Serialize};

/// Null body of command `"start"`, version 1; its response payload is [`Input`].
/// Construct with [`Start::default`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Start(Option<()>);

impl Command for Start {
    const NAME: &'static str = "start";
    const VERSION: u16 = 1;

    type Response = Input;
}
