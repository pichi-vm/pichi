// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! v0 build-port protocol on named virtio-console `dev.pichi.build` (BUILD.md §2.4).
//!
//! Bidirectional, self-delimiting CBOR, one round trip:
//! 1. Guest → host: `{"v":1,"op":"build-input"}`
//! 2. Host → guest: success (`ok:true` + build-input fields) or failure
//!    (`ok:false` + `error`).
//!
//! The host prepares the build-input payload before launch, then waits for the
//! guest request after dillo starts (hold-until-open / Linux discard).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};

pub(crate) use conglobate::build::{
    Input as BuildInput, PORT_NAME as BUILD_PORT_NAME, Request as GuestRequest, Response,
};

pub(crate) trait ResponseExt {
    fn encode_cbor(&self) -> Result<Vec<u8>>;
}
impl ResponseExt for Response {
    fn encode_cbor(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        ciborium::into_writer(self, &mut bytes).context("encoding build-port response")?;
        Ok(bytes)
    }
}

fn encode_failure(error: &str) -> Result<Vec<u8>> {
    Response::Failure(error.to_owned()).encode_cbor()
}

/// Read one guest request, validate it, write the prepared response bytes.
///
/// On a bad request (decode or validate failure), writes a v0 failure map
/// (`ok:false` + `error`) before returning `Err`, so the guest still gets a
/// self-delimiting reply.
pub(crate) fn serve_build_port(
    mut host_rx: impl Read,
    mut host_tx: impl Write,
    response: &[u8],
) -> Result<()> {
    let request: GuestRequest = match ciborium::from_reader(&mut host_rx) {
        Ok(request) => request,
        Err(err) => {
            let msg = format!("reading build-port guest request: {err}");
            write_failure_response(&mut host_tx, &msg)?;
            bail!("{msg}");
        }
    };
    if let Err(err) = request.validate() {
        write_failure_response(&mut host_tx, &err.to_string())?;
        return Err(err.into());
    }
    host_tx
        .write_all(response)
        .context("writing build-port host response")?;
    host_tx
        .flush()
        .context("flushing build-port host response")?;
    Ok(())
}

fn write_failure_response(host_tx: &mut impl Write, error: &str) -> Result<()> {
    let failure = encode_failure(error)?;
    host_tx
        .write_all(&failure)
        .context("writing build-port failure response")?;
    host_tx
        .flush()
        .context("flushing build-port failure response")?;
    Ok(())
}

/// Format the dillo `--console` value for a bidirectional `dev.pichi.build`
/// port. `rx_fd` / `tx_fd` are the **device-side** inherited pipe ends
/// (`rx` = host→guest, `tx` = guest→host; dillo polarity).
pub(crate) fn console_arg(rx_fd: u64, tx_fd: u64) -> String {
    format!("endpoint=stdio,ports=[[name={BUILD_PORT_NAME},rx=pipe:{rx_fd},tx=pipe:{tx_fd}]]")
}

/// Host-held ends of the two anonymous pipes that back the named port.
#[derive(Debug)]
pub(crate) struct BuildPortPipes {
    /// Host writes build-input response here (host→guest / device rx).
    pub host_tx: std::fs::File,
    /// Host reads the guest request here (guest→host / device tx).
    pub host_rx: std::fs::File,
    /// Device-side end for `rx=pipe:N` (dillo reads; host→guest).
    pub device_rx_fd: u64,
    /// Device-side end for `tx=pipe:N` (dillo writes; guest→host).
    pub device_tx_fd: u64,
    /// Owned device-side files kept alive until after dillo is spawned.
    pub device_rx: std::fs::File,
    pub device_tx: std::fs::File,
}

/// Create the bidirectional pipe pair for `dev.pichi.build`.
pub(crate) fn create_build_port_pipes() -> Result<BuildPortPipes> {
    let (device_rx, host_tx) = pipe_pair().context("creating host→guest build-port pipe")?;
    let (host_rx, device_tx) = pipe_pair().context("creating guest→host build-port pipe")?;
    let device_rx_fd = fd_number(&device_rx);
    let device_tx_fd = fd_number(&device_tx);
    // `std::io::pipe` ends are CLOEXEC / non-inheritable. Host ends stay that
    // way; device ends must be inheritable so dillo can adopt `pipe:N`.
    #[cfg(windows)]
    {
        set_device_inheritable(&device_rx)?;
        set_device_inheritable(&device_tx)?;
    }
    Ok(BuildPortPipes {
        host_tx,
        host_rx,
        device_rx_fd,
        device_tx_fd,
        device_rx,
        device_tx,
    })
}

fn fd_number(file: &std::fs::File) -> u64 {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        file.as_raw_fd() as u64
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        file.as_raw_handle() as usize as u64
    }
}

fn pipe_pair() -> Result<(std::fs::File, std::fs::File)> {
    let (read_end, write_end) = std::io::pipe().context("std::io::pipe")?;
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;
        Ok((
            std::fs::File::from(OwnedFd::from(read_end)),
            std::fs::File::from(OwnedFd::from(write_end)),
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::OwnedHandle;
        Ok((
            std::fs::File::from(OwnedHandle::from(read_end)),
            std::fs::File::from(OwnedHandle::from(write_end)),
        ))
    }
}

/// Mark a device-side pipe end inheritable (Windows). On Unix, inheritance is
/// handled by clearing `FD_CLOEXEC` in the child's `pre_exec` instead.
#[cfg(windows)]
fn set_device_inheritable(file: &std::fs::File) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        let handle = file.as_raw_handle();
        // SAFETY: `handle` is an owned pipe end from std::io::pipe; we only
        // set HANDLE_FLAG_INHERIT so CreateProcess can pass it to dillo.
        #[allow(unsafe_code)]
        let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("SetHandleInformation set inherit on device end");
        }
        Ok(())
    }
}

/// Clear `FD_CLOEXEC` on the device-side ends so they survive `exec` into dillo
/// under the same descriptor numbers referenced by `--console … pipe:N`.
#[cfg(unix)]
pub(crate) fn clear_device_cloexec(device_rx_fd: u64, device_tx_fd: u64) -> Result<()> {
    clear_cloexec_fd(device_rx_fd)?;
    clear_cloexec_fd(device_tx_fd)?;
    Ok(())
}

#[cfg(unix)]
fn clear_cloexec_fd(fd: u64) -> Result<()> {
    use std::os::fd::{BorrowedFd, RawFd};
    let raw = RawFd::try_from(fd).context("build-port device fd does not fit RawFd")?;
    // SAFETY: `fd` is a descriptor this process still owns (device-side pipe
    // end kept alive in `BuildPortPipes`); we only clear FD_CLOEXEC on it.
    #[allow(unsafe_code)]
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    let flags = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_GETFD)
        .map_err(std::io::Error::from)
        .context("F_GETFD on build-port device fd")?;
    let cleared = nix::fcntl::FdFlag::from_bits_truncate(flags) & !nix::fcntl::FdFlag::FD_CLOEXEC;
    nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_SETFD(cleared))
        .map_err(std::io::Error::from)
        .context("F_SETFD clear CLOEXEC on build-port device fd")?;
    Ok(())
}

/// True when `path` parses as a dillo console spec naming our build port with
/// both rx and tx `pipe:` backends (unit-test helper).
#[cfg(test)]
pub(crate) fn console_arg_names_build_port(value: &str) -> bool {
    value.contains(&format!("name={BUILD_PORT_NAME}"))
        && value.contains("rx=pipe:")
        && value.contains("tx=pipe:")
        && value.contains("endpoint=stdio")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn success_response_round_trips_required_fields() {
        let input = BuildInput {
            git_ref: "abc".into(),
            plan: "pichi.yaml".into(),
            lock: "pichi.lock".into(),
            context: None,
            location: None,
        };
        let bytes = Response::Success(input).encode_cbor().unwrap();
        let value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("expected map");
        };
        let mut got = std::collections::BTreeMap::new();
        for (k, v) in entries {
            let ciborium::Value::Text(key) = k else {
                panic!("key");
            };
            got.insert(key, v);
        }
        assert_eq!(got.len(), 5);
        assert_eq!(got["v"], ciborium::Value::Integer(1.into()));
        assert_eq!(got["ok"], ciborium::Value::Bool(true));
        assert_eq!(got["ref"], ciborium::Value::Text("abc".into()));
        assert_eq!(got["plan"], ciborium::Value::Text("pichi.yaml".into()));
        assert_eq!(got["lock"], ciborium::Value::Text("pichi.lock".into()));
        assert!(!got.contains_key("context"));
        assert!(!got.contains_key("location"));
        assert!(!got.contains_key("commit"));
    }

    #[test]
    fn success_includes_optional_fields_when_set() {
        let input = BuildInput {
            git_ref: "main".into(),
            plan: "r.yaml".into(),
            lock: "l.lock".into(),
            context: Some("crates/x".into()),
            location: Some("https://example.test/x".into()),
        };
        let bytes = Response::Success(input).encode_cbor().unwrap();
        let value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("map");
        };
        let mut got = std::collections::BTreeMap::new();
        for (k, v) in entries {
            let ciborium::Value::Text(key) = k else {
                panic!("key");
            };
            got.insert(key, v);
        }
        assert_eq!(got.len(), 7);
        assert_eq!(got["plan"], ciborium::Value::Text("r.yaml".into()));
        assert_eq!(got["lock"], ciborium::Value::Text("l.lock".into()));
        assert_eq!(got["context"], ciborium::Value::Text("crates/x".into()));
        assert_eq!(
            got["location"],
            ciborium::Value::Text("https://example.test/x".into())
        );
    }

    #[test]
    fn failure_response_shape() {
        let bytes = encode_failure("cache missing").unwrap();
        let value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("map");
        };
        let mut got = std::collections::BTreeMap::new();
        for (k, v) in entries {
            let ciborium::Value::Text(key) = k else {
                panic!("key");
            };
            got.insert(key, v);
        }
        assert_eq!(got["v"], ciborium::Value::Integer(1.into()));
        assert_eq!(got["ok"], ciborium::Value::Bool(false));
        assert_eq!(got["error"], ciborium::Value::Text("cache missing".into()));
    }

    #[test]
    fn serve_accepts_expected_request_and_writes_response() {
        let req = {
            let mut b = Vec::new();
            ciborium::into_writer(&GuestRequest::default(), &mut b).unwrap();
            b
        };
        let response = encode_failure("nope").unwrap();
        let mut rx = Cursor::new(req);
        let mut tx = Vec::new();
        serve_build_port(&mut rx, &mut tx, &response).unwrap();
        assert_eq!(tx, response);
    }

    #[test]
    fn serve_rejects_wrong_op() {
        let bad = GuestRequest {
            v: 1,
            op: "other".into(),
        };
        let mut b = Vec::new();
        ciborium::into_writer(&bad, &mut b).unwrap();
        let mut tx = Vec::new();
        let err = serve_build_port(Cursor::new(b), &mut tx, b"x").unwrap_err();
        assert!(
            err.to_string().contains("unknown op"),
            "unexpected error: {err}"
        );
        let expected = encode_failure(&err.to_string()).unwrap();
        assert_eq!(tx, expected);
    }

    #[test]
    fn serve_writes_failure_on_decode_error() {
        let mut tx = Vec::new();
        let err = serve_build_port(Cursor::new(b"\xffnot-cbor"), &mut tx, b"x").unwrap_err();
        assert!(
            err.to_string().contains("reading build-port guest request"),
            "unexpected error: {err}"
        );
        assert!(!tx.is_empty());
        let value: ciborium::Value = ciborium::from_reader(tx.as_slice()).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("map");
        };
        let mut got = std::collections::BTreeMap::new();
        for (k, v) in entries {
            let ciborium::Value::Text(key) = k else {
                panic!("key");
            };
            got.insert(key, v);
        }
        assert_eq!(got["v"], ciborium::Value::Integer(1.into()));
        assert_eq!(got["ok"], ciborium::Value::Bool(false));
        assert!(matches!(got.get("error"), Some(ciborium::Value::Text(_))));
    }

    #[test]
    fn console_arg_is_bidirectional_named_port() {
        let s = console_arg(3, 4);
        assert!(console_arg_names_build_port(&s), "{s}");
        assert!(s.contains("rx=pipe:3"));
        assert!(s.contains("tx=pipe:4"));
        // Must parse as dillo ConsoleSpec.
        let spec: dillo_config::ConsoleSpec =
            serde_keyvalue::from_key_values(&s).expect("console parses");
        assert_eq!(spec.endpoint.as_deref(), Some("stdio"));
        assert_eq!(spec.ports.len(), 1);
        assert_eq!(spec.ports[0].name, BUILD_PORT_NAME);
        assert!(spec.ports[0].rx.is_some());
        assert!(spec.ports[0].tx.is_some());
    }

    #[test]
    fn create_pipes_assigns_distinct_device_fds() {
        let pipes = create_build_port_pipes().unwrap();
        assert_ne!(pipes.device_rx_fd, pipes.device_tx_fd);
        let s = console_arg(pipes.device_rx_fd, pipes.device_tx_fd);
        assert!(console_arg_names_build_port(&s), "{s}");
    }
}
