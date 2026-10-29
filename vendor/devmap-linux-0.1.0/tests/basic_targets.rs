// SPDX-License-Identifier: Apache-2.0

mod common;

use std::{
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom, Write},
};

use devmap_core::Span;
use devmap_linux::targets::{Error, Linear};
use devmap_linux::{
    Control, DevId, Table, records::Records, targets::Parse, targets::Target, targets::Version,
};

use common::{LoopDevice, Owned};

const VERSION: Version = Version {
    major: 1,
    minor: 0,
    patch: 0,
};

#[test]
fn codecs_validate_parameters_versions_and_destination_errors() {
    let target = Linear {
        device: DevId::new(7, 1).unwrap(),
        offset: 2048,
    };
    let mut bytes = Vec::new();
    target.encode(VERSION, &mut bytes).unwrap();
    assert_eq!(bytes, b"7:1 2048");
    assert_eq!(Linear::parse(" \t7:1 2048\n", VERSION).unwrap(), target);
    for text in [
        "",
        "7:1",
        "7:1 -1",
        "7:1 18446744073709551616",
        "4096:1 0",
        "7:1 0 extra",
    ] {
        assert_eq!(
            Linear::parse(text, VERSION).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    assert_eq!(
        Linear::parse("7:1 18446744073709551615", VERSION)
            .unwrap()
            .offset,
        u64::MAX
    );
    let mut buffer = [0; 2];
    let mut output = buffer.as_mut_slice();
    assert_eq!(
        target.encode(VERSION, &mut output).unwrap_err().kind(),
        io::ErrorKind::WriteZero
    );
    let unsupported = Version {
        major: 2,
        ..VERSION
    };
    assert_eq!(
        target.encode(unsupported, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        Linear::parse("7:1 0", unsupported).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        Error.encode(unsupported, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    bytes.clear();
    Error.encode(VERSION, &mut bytes).unwrap();
    assert_eq!(bytes, [] as [u8; 0]);
    <Error as Target>::Table::parse("", VERSION).unwrap();
    assert!(<Error as Target>::Table::parse("7:1 0", VERSION).is_err());
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn linear_maps_reads_and_writes_at_the_backing_offset() {
    let control = Control::open().expect("open /dev/mapper/control");
    let backing = LoopDevice::create("linear", 1024 * 1024);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&backing.path)
        .unwrap();
    file.seek(SeekFrom::Start(4096)).unwrap();
    file.write_all(&[0x5a; 512]).unwrap();
    file.sync_all().unwrap();
    let target = Linear {
        device: DevId::try_from(file.metadata().unwrap()).unwrap(),
        offset: 8,
    };
    let version = control.version(Linear::NAME).unwrap();
    let dev = Owned::create(
        &control,
        &format!("devmap-test-linear-{}", std::process::id()),
    )
    .unwrap();
    control
        .load(
            dev.id(),
            Table::<4096>::default()
                .add(Span::new(0, 8), &target, version)
                .unwrap(),
            false,
        )
        .unwrap();
    control.suspend(dev.id(), false).unwrap();
    let mut mapped = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev.node_path())
        .unwrap();
    let mut bytes = [0; 512];
    mapped.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0x5a; 512]);
    mapped.write_all(&[0xa5; 512]).unwrap();
    mapped.sync_all().unwrap();
    // Writes through the mapping do not invalidate the backing device's page cache.
    assert!(
        std::process::Command::new("blockdev")
            .arg("--flushbufs")
            .arg(&backing.path)
            .status()
            .unwrap()
            .success()
    );
    file.seek(SeekFrom::Start(4608)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0xa5; 512]);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 512]);
    let mut rows = control.table::<4096>(dev.id()).unwrap();
    assert_eq!(
        rows.next()
            .unwrap()
            .unwrap()
            .parse::<Linear>(version)
            .unwrap(),
        target
    );
    assert!(rows.next().is_none());
    let mut info = control.info::<4096>(dev.id()).unwrap();
    info.next()
        .unwrap()
        .unwrap()
        .parse::<Linear>(version)
        .unwrap();
    let mut deps = control.deps::<4096>(dev.id()).unwrap();
    assert_eq!(deps.next().unwrap().unwrap(), target.device);
    assert!(deps.next().is_none());
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn error_fails_io_and_reports_empty_parameters() {
    let control = Control::open().expect("open /dev/mapper/control");
    let version = control.version(Error::NAME).unwrap();
    let dev = Owned::create(
        &control,
        &format!("devmap-test-error-{}", std::process::id()),
    )
    .unwrap();
    control
        .load(
            dev.id(),
            Table::<4096>::default()
                .add(Span::new(0, 8), &Error, version)
                .unwrap(),
            false,
        )
        .unwrap();
    control.suspend(dev.id(), false).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev.node_path())
        .unwrap();
    assert_eq!(
        file.read(&mut [0; 512]).unwrap_err().raw_os_error(),
        Some(5)
    );
    assert_eq!(
        file.write_all(&[0; 512])
            .and_then(|()| file.sync_all())
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
    let mut rows = control.table::<4096>(dev.id()).unwrap();
    rows.next()
        .unwrap()
        .unwrap()
        .parse::<Error>(version)
        .unwrap();
    let mut info = control.info::<4096>(dev.id()).unwrap();
    info.next()
        .unwrap()
        .unwrap()
        .parse::<Error>(version)
        .unwrap();
    assert!(control.deps::<4096>(dev.id()).unwrap().next().is_none());
}
