// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]

use std::io;

use devmap_linux::{DevId, targets::Target, targets::Version};
use devmap_snapshot::Origin;

#[test]
fn encode_preserves_version_and_destination_errors() {
    struct Reject;
    impl io::Write for Reject {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(28))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let target = Origin {
        origin: DevId::new(252, 1).unwrap(),
    };
    let version = Version {
        major: 2,
        minor: 0,
        patch: 0,
    };
    let error = target.encode(version, &mut Reject).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("2.0.0"));
    assert!(
        matches!(target.encode(Version { major: 1, minor: 0, patch: 0 }, &mut Reject),
        Err(error) if error.raw_os_error() == Some(28))
    );
    let mut text = Vec::new();
    target
        .encode(
            Version {
                major: 1,
                minor: 0,
                patch: 0,
            },
            &mut text,
        )
        .unwrap();
    assert_eq!(text, b"252:1");
}
