// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use carapace::image::{Plan as _, Planned};
use std::io::{Read as _, Write as _};

/// Small in-memory fixtures for command tests that need a valid base scute.
pub(super) trait ScuteFixture {
    fn scute(&self) -> Vec<u8>;
}

impl ScuteFixture for [u8] {
    fn scute(&self) -> Vec<u8> {
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(self).unwrap();
        let mut planned = Planned::plan(source).unwrap();
        let mut cow = Vec::new();
        planned.read_to_end(&mut cow).unwrap();
        cow
    }
}
