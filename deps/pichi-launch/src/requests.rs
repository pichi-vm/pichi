// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Finish, Request, Start};
use serde::{Deserialize, Serialize};

/// All requests understood by the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Requests {
    Start(Request<Start>),

    Finish(Request<Finish>),
}
