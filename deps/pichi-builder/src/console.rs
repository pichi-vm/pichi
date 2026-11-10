// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Open the named virtio console via sysfs virtio-ports. No udev symlink dependency.
//!
//! Build port: guest opens `dev.pichi.build` read/write, writes one CBOR
//! request array, then reads one self-delimiting CBOR response. The guest must
//! not assume the host already pushed bytes before the request.

#[cfg(test)]
use ciborium::Value;
use pichi_launch::{Finish, PORT_NAME as CONSOLE_NAME, Request, Response, Start};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConsoleError {
    #[error("console I/O: {0}")]
    Io(#[from] io::Error),
    #[error("build console {CONSOLE_NAME} was not found under sysfs virtio-ports")]
    NotFound,
    #[error("build-port encode: {0}")]
    Encode(String),
    #[error("finish acknowledgement: {0}")]
    Finish(String),
}

/// Locate `<dev_root>/<port>` for the port whose sysfs `name` equals `dev.pichi.build`.
///
/// Opens the device read/write. `sysfs_root` is typically `/sys` and `dev_root`
/// typically `/dev`. Tests inject temporary trees for both.
pub(crate) fn open_build_console(
    sysfs_root: &Path,
    dev_root: &Path,
) -> Result<fs::File, ConsoleError> {
    let ports = sysfs_root.join("class/virtio-ports");
    let mut entries = fs::read_dir(&ports).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ConsoleError::NotFound
        } else {
            ConsoleError::Io(error)
        }
    })?;
    while let Some(entry) = entries.next().transpose()? {
        let path = entry.path();
        let name_path = path.join("name");
        let Ok(name) = fs::read_to_string(&name_path) else {
            continue;
        };
        let name = name.trim_end_matches(['\n', '\r']);
        if name != CONSOLE_NAME {
            continue;
        }
        let dev_name = path.file_name().ok_or(ConsoleError::NotFound)?;
        let device = dev_root.join(dev_name);
        return Ok(fs::OpenOptions::new().read(true).write(true).open(device)?);
    }
    Err(ConsoleError::NotFound)
}

/// Encode the guest→host request: `["start", 1, null]`.
pub(crate) fn encode_build_input_request() -> Result<Vec<u8>, ConsoleError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&Request(Start::default()), &mut bytes)
        .map_err(|error| ConsoleError::Encode(error.to_string()))?;
    Ok(bytes)
}

/// Write the build-input request to the console (guest → host).
pub(crate) fn write_build_input_request(file: &mut impl Write) -> Result<(), ConsoleError> {
    let bytes = encode_build_input_request()?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}

/// Report completion only after the pipeline has finalized its output or failed.
pub(crate) fn write_build_result(
    file: &mut (impl Read + Write),
    result: Result<(), String>,
) -> Result<(), ConsoleError> {
    ciborium::into_writer(&Request(Finish(result.err())), &mut *file)
        .map_err(|error| ConsoleError::Encode(error.to_string()))?;
    file.flush()?;
    let response: Response<Option<()>> =
        ciborium::from_reader(file).map_err(|error| ConsoleError::Finish(error.to_string()))?;
    match response {
        Response::Ok(_) => Ok(()),
        Response::Err(error) => Err(ConsoleError::Finish(error)),
    }
}

/// Read one self-delimiting CBOR value from the console (host → guest).
///
/// Does not read to EOF: the host may leave the port open after the response.
/// Test helper: production reads via [`crate::response`].
#[cfg(test)]
pub(crate) fn read_cbor_value(file: &mut impl std::io::Read) -> Result<Value, ConsoleError> {
    ciborium::from_reader(file)
        .map_err(|error| ConsoleError::Io(io::Error::other(error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        os::unix::net::UnixStream,
        sync::{Arc, Barrier},
        thread,
    };

    fn fixture_named_port(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let sysfs = root.join("sys");
        let dev = root.join("dev");
        let ports = sysfs.join("class/virtio-ports");
        fs::create_dir_all(ports.join("vport0p1")).unwrap();
        fs::create_dir_all(ports.join("vport0p2")).unwrap();
        fs::create_dir_all(&dev).unwrap();
        fs::write(ports.join("vport0p1/name"), "other\n").unwrap();
        fs::write(ports.join("vport0p2/name"), "dev.pichi.build\n").unwrap();
        (sysfs, dev)
    }

    #[test]
    fn encode_request_is_start() {
        let bytes = encode_build_input_request().unwrap();
        let value: Value = ciborium::from_reader(Cursor::new(&bytes)).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                Value::Text("start".into()),
                Value::Integer(1.into()),
                Value::Null
            ])
        );
        assert_eq!(bytes, b"\x83\x65start\x01\xf6");
    }

    #[test]
    fn finish_reports_success_or_original_failure_and_waits_for_acknowledgement() {
        for result in [
            Ok(()),
            Err("rootfs command 1: apt exited with status 100".into()),
        ] {
            let (mut guest, mut host) = UnixStream::pair().unwrap();
            let expected = result.clone().err();
            let server = thread::spawn(move || {
                let request: pichi_launch::Requests = ciborium::from_reader(&mut host).unwrap();
                assert_eq!(
                    request,
                    pichi_launch::Requests::Finish(Request(Finish(expected)))
                );
                ciborium::into_writer(&Response::Ok(None::<()>), &mut host).unwrap();
            });
            write_build_result(&mut guest, result).unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn opens_named_console_read_write() {
        let root = tempfile::TempDir::new().unwrap();
        let (sysfs, dev) = fixture_named_port(root.path());
        // Regular file stands in for the device node; open must succeed RDWR.
        fs::write(dev.join("vport0p2"), []).unwrap();
        let mut file = open_build_console(&sysfs, &dev).unwrap();
        assert!(
            rustix::io::fcntl_getfd(&file)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        write_build_input_request(&mut file).unwrap();
        let written = fs::read(dev.join("vport0p2")).unwrap();
        assert_eq!(written, encode_build_input_request().unwrap());
    }

    #[test]
    fn exchange_writes_request_then_reads_response() {
        let (mut guest, mut host) = UnixStream::pair().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let host_barrier = Arc::clone(&barrier);
        let host_thread = thread::spawn(move || {
            host_barrier.wait();
            let request = read_cbor_value(&mut host).unwrap();
            assert_eq!(
                request,
                Value::Array(vec![
                    Value::Text("start".into()),
                    Value::Integer(1.into()),
                    Value::Null
                ])
            );
            let response = pichi_launch::Response::Ok(pichi_launch::Input {
                r#ref: "refs/heads/main".into(),
                plan: "plan.yml".into(),
                lock: "lock.yml".into(),
                context: ".".into(),
                location: None,
            });
            ciborium::into_writer(&response, &mut host).unwrap();
        });
        barrier.wait();
        write_build_input_request(&mut guest).unwrap();
        let response = read_cbor_value(&mut guest).unwrap();
        host_thread.join().unwrap();
        let Value::Array(fields) = response else {
            panic!("response array");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], Value::Bool(true));
        assert!(matches!(fields[1], Value::Map(_)));
    }

    #[test]
    fn finds_named_port_but_missing_device_is_io_error() {
        let root = tempfile::TempDir::new().unwrap();
        let (sysfs, dev) = fixture_named_port(root.path());
        let error = open_build_console(&sysfs, &dev).unwrap_err().to_string();
        assert!(
            error.contains("No such file") || error.contains("os error"),
            "{error}"
        );
    }

    #[test]
    fn missing_ports_directory_is_not_found() {
        let root = tempfile::TempDir::new().unwrap();
        let error = open_build_console(root.path(), Path::new("/dev")).unwrap_err();
        assert!(matches!(error, ConsoleError::NotFound), "{error:?}");
    }
}
