// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::Command;
use serde::{Deserialize, Serialize};

/// Report a completed build: null on success, or the failure message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Finish(pub Option<String>);

impl Command for Finish {
    const NAME: &'static str = "finish";
    const VERSION: u16 = 1;

    type Response = Option<()>;
}
