// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Select a fixture executable without hiding the monitor or system tools.

pub(crate) fn prepend(directory: &std::path::Path) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(directory.to_owned()).chain(std::env::split_paths(&path)))
        .unwrap()
}
