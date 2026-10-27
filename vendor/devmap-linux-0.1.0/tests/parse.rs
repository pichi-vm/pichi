// SPDX-License-Identifier: Apache-2.0

use devmap_linux::{DevId, targets::Parse, targets::Version};

#[test]
fn identifiers_round_trip() {
    for (major, minor) in [(252, 5), (1, 0x1_2345)] {
        let id = DevId::new(major, minor).unwrap();
        assert_eq!(id.to_string().parse::<DevId>().unwrap(), id);
    }
    assert!(DevId::new(4096, 0).is_none());
    assert!(DevId::new(0, 1 << 20).is_none());
    for text in ["", "1", "1:2:3", "4096:0", "0:1048576"] {
        assert_eq!(
            text.parse::<DevId>().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn empty_status_rejects_unexpected_text() {
    let version = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };
    <() as Parse>::parse("", version).unwrap();
    <() as Parse>::parse(" \t\n", version).unwrap();
    assert_eq!(
        <() as Parse>::parse("unexpected", version)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[cfg(target_os = "linux")]
#[test]
fn identifiers_reject_regular_files() {
    assert_eq!(
        DevId::try_from(std::fs::metadata(std::env::current_exe().unwrap()).unwrap())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(
        DevId::try_from(file).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}
