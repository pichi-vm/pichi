// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::unwrap_used)]
use devmap_snapshot::{Exception, Header};

#[test]
fn header_records_and_layout() {
    let header = Header {
        valid: true,
        chunk_size: devmap_core::BlockSize::default(),
    };
    let expected = [0x53, 0x6e, 0x41, 0x70, 1, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0];
    assert_eq!(header.encode(), expected);
    assert_eq!(Header::decode(expected).unwrap(), header);
    assert_eq!(header.exceptions_per_chunk(), 256);
    assert_eq!(header.metadata_chunk_index(0).unwrap(), 1);
    assert_eq!(header.metadata_chunk_index(1).unwrap(), 258);
    assert!(header.metadata_chunk_index(u64::MAX).is_err());
    let mut invalidated = expected;
    invalidated[4] = 0;
    let invalidated_header = Header {
        valid: false,
        ..header
    };
    assert_eq!(Header::decode(invalidated).unwrap(), invalidated_header);
    assert_eq!(invalidated_header.encode(), invalidated);
    for (offset, value) in [(0, 0), (4, 2), (8, 2), (12, 0), (12, 3)] {
        let mut bytes = expected;
        bytes[offset] = value;
        assert_eq!(
            Header::decode(bytes).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn exception_records_include_terminators() {
    for record in [
        Exception {
            old_chunk: 7,
            new_chunk: 2,
        },
        Exception {
            old_chunk: u64::MAX,
            new_chunk: 0,
        },
    ] {
        assert_eq!(Exception::decode(record.encode()), record);
    }
    let bytes = Exception {
        old_chunk: 7,
        new_chunk: 2,
    }
    .encode();
    assert_eq!(bytes, [7, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
}
