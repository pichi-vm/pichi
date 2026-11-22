// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use conglobate::artifact::{ConfigDescriptor, Digest};
use sha2::{Digest as _, Sha256};

/// A small valid stored config for fixtures without launch requirements.
pub(super) trait ConfigFixture {
    const BYTES: &'static [u8] = br#"{"version":1}"#;
    fn fixture() -> Self;
}

impl ConfigFixture for ConfigDescriptor {
    fn fixture() -> Self {
        Self::for_config(
            Digest::Sha256(Sha256::digest(Self::BYTES).into()),
            Self::BYTES.len() as u64,
        )
    }
}
