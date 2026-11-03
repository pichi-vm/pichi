// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Build, Request};
use serde::{Deserialize, Serialize};

/// All requests understood by the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Requests {
    Build(Request<Build>),
}
