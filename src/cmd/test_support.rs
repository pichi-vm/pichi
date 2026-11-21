// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use carapace::image::{Plan as _, Planned};
use std::io::{Read as _, Write as _};

/// Small in-memory fixtures for command tests that need a valid base scute.
pub(super) trait ScuteFixture {
    fn scute(&self) -> Vec<u8>;
    fn descriptor(&self, salt: &[u8], compressed: bool) -> conglobate::artifact::ScuteDescriptor;
}

impl ScuteFixture for [u8] {
    fn descriptor(&self, salt: &[u8], compressed: bool) -> conglobate::artifact::ScuteDescriptor {
        use conglobate::artifact::{Digest, ScuteAnnotations, ScuteDescriptor};
        use sha2::{Digest as _, Sha256};
        let tree = super::verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt.to_vec(),
            uuid: [0; 16],
        }
        .compute(self)
        .unwrap();
        let wire = if compressed {
            ruzstd::encoding::compress_to_vec(self, ruzstd::encoding::CompressionLevel::Fastest)
        } else {
            self.to_vec()
        };
        ScuteDescriptor {
            digest: Digest::Sha256(Sha256::digest(&wire).into()).to_string(),
            size: wire.len() as u64,
            annotations: ScuteAnnotations {
                digest: Digest::Sha256(Sha256::digest(self).into()).to_string(),
                size: self.len().to_string(),
                data_blocks: (self.len() as u64).div_ceil(4096).try_into().unwrap(),
                salt: hex::encode(salt),
                verity_digest: Digest::Sha256(Sha256::digest(&tree.blob).into()).to_string(),
            },
        }
    }

    fn scute(&self) -> Vec<u8> {
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(self).unwrap();
        let mut planned = Planned::plan(source).unwrap();
        let mut cow = Vec::new();
        planned.read_to_end(&mut cow).unwrap();
        cow
    }
}
