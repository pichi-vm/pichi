// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// Locators supplied by the host. Validation and resolution are builder work.
/// Every path must resolve inside the context mount, including through symlinks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Git reference or commit ID to build.
    pub r#ref: String,
    /// Plan file relative to the selected context.
    pub plan: String,
    /// Lock file relative to the selected context.
    pub lock: String,
    /// Directory within the mounted repository; `"."` explicitly selects its root.
    pub context: String,
    /// Optional source provenance locator. Omitted or null means absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}
