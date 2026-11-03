// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

/// A command version and its corresponding response payload.
pub trait Command {
    const NAME: &'static str;
    const VERSION: u16;

    type Response;
}
