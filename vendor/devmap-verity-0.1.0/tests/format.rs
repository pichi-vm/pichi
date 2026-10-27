// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Path 2: format a verity hash volume compatible with `veritysetup`.

#[cfg(target_os = "linux")]
use std::{
    fmt::Write as _,
    fs::{self, File},
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    process::Command,
};
use std::{
    io::{Cursor, ErrorKind},
    num::NonZero,
};

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{Algorithm, Format as _, HashType, Header, Parameters, Salt};

#[test]
fn oversized_data_extent_is_rejected_before_writing() {
    let header = Header {
        uuid: [0; 16],
        parameters: Parameters {
            hash_type: HashType::Normal,
            algorithm: Algorithm::Sha256,
            salt: Salt::default(),
            data: Geometry {
                size: BlockSize::default(),
                count: NonZero::new(u64::MAX).unwrap(),
            },
            hash: BlockSize::default(),
        },
    };
    let mut output = Cursor::new(Vec::new());

    let error = header.format(std::io::empty(), &mut output).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(output.into_inner(), []);
}

#[test]
#[cfg(target_os = "linux")]
fn matches_veritysetup_byte_for_byte() {
    for algorithm in [Algorithm::Sha256, Algorithm::Sha224] {
        for hash_type in [HashType::Normal, HashType::ChromeOs] {
            compare_with_veritysetup(algorithm, hash_type);
        }
    }
}

#[cfg(target_os = "linux")]
fn compare_with_veritysetup(algorithm: Algorithm, hash_type: HashType) {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let data_path = directory.path().join("data.img");
    let our_hash_path = directory.path().join("ours.img");
    let reference_hash_path = directory.path().join("veritysetup.img");

    // Both formatters consume the same deterministic protected data.
    let mut data = File::options()
        .write(true)
        .create_new(true)
        .open(&data_path)
        .unwrap();
    // More than one leaf hash block exercises padding and parent hashing.
    // SHA-224 additionally distinguishes packed format-0 digests from the
    // power-of-two slots used by format 1.
    data.write_all(&vec![0x5a; 33 * 512]).unwrap();
    drop(data);

    // Set a small geometry so this fixture crosses a hash-tree level.
    let mut data = File::open(&data_path).unwrap();
    let mut hash = File::options()
        .write(true)
        .create_new(true)
        .open(&our_hash_path)
        .unwrap();
    let header = Header {
        uuid: [0x5a; 16],
        parameters: Parameters {
            hash_type,
            algorithm,
            salt: Salt::new(&[1, 2, 3]).unwrap(),
            data: Geometry {
                size: BlockSize::from_exponent(9).unwrap(),
                count: NonZero::new(33).unwrap(),
            },
            hash: BlockSize::from_exponent(9).unwrap(),
        },
    };
    let total_size = header.total_size().unwrap();

    // Our formatter writes the header and tree and returns the trusted root.
    let root = header.format(&mut data, &mut hash).unwrap();
    hash.flush().unwrap();
    drop(hash);

    // Ask the external reference implementation to format the same volume
    // with exactly the policy recorded in our header.
    let output = Command::new("veritysetup")
        .args([
            "format",
            "--data-block-size=512",
            "--hash-block-size=512",
            "--salt=010203",
            "--uuid=5a5a5a5a-5a5a-5a5a-5a5a-5a5a5a5a5a5a",
        ])
        .arg(format!("--format={hash_type}"))
        .arg(format!("--hash={algorithm}"))
        .arg(&data_path)
        .arg(&reference_hash_path)
        .output()
        .expect("veritysetup must be installed for the format compatibility test");
    assert!(
        output.status.success(),
        "veritysetup format failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Compatibility covers the complete image: header, padding, and tree.
    let ours = fs::read(our_hash_path).unwrap();
    let reference = fs::read(reference_hash_path).unwrap();
    assert_eq!(ours, reference);
    assert_eq!(ours.len() as u64, total_size);

    // The independently stored root must agree as well.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let reference_root = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Root hash:"))
        .map(str::trim)
        .expect("veritysetup output must report a root hash");

    let mut encoded_root = String::with_capacity(root.len() * 2);
    for byte in root {
        write!(encoded_root, "{byte:02x}").unwrap();
    }

    assert_eq!(encoded_root, reference_root);
}
