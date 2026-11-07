// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::build::Build;
use anyhow::{Context, Result, bail, ensure};
use pichi_launch::{Input, PORT_NAME, Request, Response};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::Path,
};

/// The named bidirectional virtio console, closed before the engine starts.
pub(crate) struct Console(File);

impl Console {
    pub(crate) fn open(sysfs: &Path, devices: &Path) -> Result<Self> {
        for entry in fs::read_dir(sysfs.join("class/virtio-ports"))? {
            let entry = entry?;
            let Ok(name) = fs::read_to_string(entry.path().join("name")) else {
                continue;
            };
            if name.trim_end_matches(['\n', '\r']) == PORT_NAME {
                return Ok(Self(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(devices.join(entry.file_name()))?,
                ));
            }
        }
        bail!("build console {PORT_NAME} was not found")
    }

    pub(crate) fn request(mut self) -> Result<Build> {
        ciborium::into_writer(&Request::from(pichi_launch::Build::default()), &mut self.0)
            .context("encoding build-input request")?;
        let response: Response<Input> =
            ciborium::from_reader(&mut self.0).context("decoding build-input response")?;
        // Do not block waiting for a second response or EOF on the console.
        let mut descriptors = [PollFd::new(&self.0, PollFlags::IN)];
        poll(&mut descriptors, Some(&Timespec::default()))?;
        let events = descriptors[0].revents();
        ensure!(
            !events.intersects(PollFlags::ERR | PollFlags::NVAL),
            "build console poll failed"
        );
        if events.intersects(PollFlags::IN | PollFlags::HUP) {
            ensure!(self.0.read(&mut [0])? == 0, "trailing build-input bytes");
        }
        match response {
            Response::Ok(input) => Ok(Build(input)),
            Response::Err(error) => bail!("host rejected build-input: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::{fd::OwnedFd, unix::net::UnixStream},
        thread,
        time::Duration,
    };

    struct Exchange;

    impl Exchange {
        fn input() -> Input {
            Input {
                r#ref: "HEAD".into(),
                context: ".".into(),
                plan: "pichi.yaml".into(),
                lock: "pichi.lock".into(),
                location: None,
            }
        }

        fn response(input: Input) -> Vec<u8> {
            let mut bytes = Vec::new();
            ciborium::into_writer(&Response::Ok(input), &mut bytes).unwrap();
            bytes
        }

        fn run(bytes: Vec<u8>) -> Result<Build> {
            let (guest, mut host) = UnixStream::pair().unwrap();
            guest
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (done, wait) = std::sync::mpsc::channel();
            let thread = thread::spawn(move || {
                let request: Request<pichi_launch::Build> =
                    ciborium::from_reader(&mut host).unwrap();
                assert_eq!(request, Request::from(pichi_launch::Build::default()));
                host.write_all(&bytes).unwrap();
                // Keep the port open until the guest has decoded the response.
                let _ = wait.recv_timeout(Duration::from_secs(3));
            });
            let result = Console(File::from(OwnedFd::from(guest))).request();
            done.send(()).unwrap();
            thread.join().unwrap();
            result
        }
    }

    #[test]
    fn requests_before_reading_and_finishes_without_eof() {
        let input = Exchange::input();
        let build = Exchange::run(Exchange::response(input.clone())).unwrap();
        assert_eq!(build.0, input);
    }

    #[test]
    fn rejects_host_failure_and_malformed_responses() {
        let mut failure = Vec::new();
        ciborium::into_writer(
            &Response::<Input>::Err("missing inputs".into()),
            &mut failure,
        )
        .unwrap();
        assert!(
            Exchange::run(failure)
                .err()
                .unwrap()
                .to_string()
                .contains("missing inputs")
        );
        for bytes in [vec![0xff], vec![0x82], vec![0x82, 0xf5, 0xa0]] {
            assert!(Exchange::run(bytes).is_err());
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = Exchange::response(Exchange::input());
        bytes.push(0xff);
        assert!(
            Exchange::run(bytes)
                .err()
                .unwrap()
                .to_string()
                .contains("trailing")
        );
    }

    #[test]
    fn discovers_exact_port_name_without_udev() {
        let root = tempfile::tempdir().unwrap();
        let ports = root.path().join("class/virtio-ports");
        let devices = root.path().join("dev");
        fs::create_dir_all(ports.join("vport0p1")).unwrap();
        fs::create_dir_all(&devices).unwrap();
        fs::write(ports.join("vport0p1/name"), format!("{PORT_NAME}-other\n")).unwrap();
        fs::write(devices.join("vport0p1"), []).unwrap();
        assert!(Console::open(root.path(), &devices).is_err());
        fs::write(ports.join("vport0p1/name"), format!("{PORT_NAME}\n")).unwrap();
        let mut console = Console::open(root.path(), &devices).unwrap();
        console.0.write_all(b"request").unwrap();
        assert_eq!(fs::read(devices.join("vport0p1")).unwrap(), b"request");
    }
}
