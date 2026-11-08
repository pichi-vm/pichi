// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Guest console exchange; Conglobate validates and executes the build inputs.

use anyhow::{Context, Result, bail, ensure};
use pichi_launch::{Finish, Input, PORT_NAME, Request, Response, Start};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde::de::DeserializeOwned;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    path::Path,
    process::Command,
};

#[derive(Debug)]
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
        bail!("build console {PORT_NAME} not found")
    }

    pub(crate) fn run(&mut self, builder: Command) -> Result<()> {
        ciborium::into_writer(&Request(Start::default()), &mut self.0)?;
        self.0.flush()?;
        let result = self.build(builder);
        let reported = self.finish(result.as_ref().err().map(|error| format!("{error:#}")));
        // Keep the original build error if reporting it also fails.
        result?;
        reported
    }

    fn build(&mut self, mut builder: Command) -> Result<()> {
        let input: Input = self.response().context("receiving build inputs")?;
        builder.args([
            "build",
            "/run/pichi/repo",
            "--cache=/run/pichi/cache",
            "--output=/run/pichi/output",
            "--uid-map=100000:65536",
            "--gid-map=100000:65536",
        ]);
        builder
            .arg(format!("--ref={}", input.r#ref))
            .arg(format!("--context={}", input.context))
            .arg(format!("--plan={}", input.plan))
            .arg(format!("--lock={}", input.lock));
        if let Some(location) = input.location {
            builder.arg(format!("--location={location}"));
        }
        let status = builder.status().context("starting conglobate build")?;
        ensure!(status.success(), "conglobate build: {status}");
        Ok(())
    }

    fn finish(&mut self, error: Option<String>) -> Result<()> {
        ciborium::into_writer(&Request(Finish(error)), &mut self.0)?;
        self.0.flush()?;
        self.response::<Option<()>>()
            .context("receiving finish acknowledgement")?;
        Ok(())
    }

    fn response<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut reader = BufReader::new(&mut self.0);
        let response: Response<T> = ciborium::from_reader(&mut reader)?;
        ensure!(reader.buffer().is_empty(), "trailing build response bytes");
        // The port stays open between messages. Check queued bytes without
        // waiting for EOF or the next response.
        let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
        poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        let events = fds[0].revents();
        ensure!(
            !events.intersects(PollFlags::ERR | PollFlags::NVAL),
            "build console poll failed"
        );
        if events.intersects(PollFlags::IN | PollFlags::HUP) {
            ensure!(self.0.read(&mut [0])? == 0, "trailing build response bytes");
        }
        match response {
            Response::Ok(value) => Ok(value),
            Response::Err(error) => bail!("host rejected request: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pichi_launch::Requests;
    use std::{os::unix::net::UnixStream, thread, time::Duration};

    fn pair() -> (Console, UnixStream) {
        let (guest, host) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (Console(File::from(std::os::fd::OwnedFd::from(guest))), host)
    }

    fn input() -> Input {
        Input {
            r#ref: "refs/heads/main".into(),
            plan: "a plan.yaml".into(),
            lock: "a lock.yaml".into(),
            context: "project $(untouched)".into(),
            location: Some("https://example.invalid/repo".into()),
        }
    }

    #[test]
    fn builder_arguments_and_result_cross_the_console_in_order() {
        for code in [0, 7] {
            let root = tempfile::tempdir().unwrap();
            let args = root.path().join("arguments");
            let recorded = args.clone();
            let (mut guest, mut host) = pair();
            let server = thread::spawn(move || {
                let start: Requests = ciborium::from_reader(&mut host).unwrap();
                assert_eq!(start, Requests::Start(Request(Start::default())));
                ciborium::into_writer(&Response::Ok(input()), &mut host).unwrap();
                let finish: Requests = ciborium::from_reader(&mut host).unwrap();
                let Requests::Finish(Request(Finish(error))) = finish else {
                    panic!("expected finish")
                };
                assert_eq!(error.is_none(), code == 0);
                assert!(recorded.is_file(), "finish must follow the builder");
                ciborium::into_writer(&Response::Ok(None::<()>), &mut host).unwrap();
            });
            let mut builder = Command::new("sh");
            builder
                .args([
                    "-c",
                    "printf '%s\\n' \"$@\" > \"$ARGS\"; exit \"$STATUS\"",
                    "builder",
                ])
                .env("ARGS", &args)
                .env("STATUS", code.to_string());
            let result = guest.run(builder);
            assert_eq!(result.is_ok(), code == 0, "{result:?}");
            server.join().unwrap();
            let arguments = fs::read_to_string(args).unwrap();
            assert_eq!(
                arguments.lines().collect::<Vec<_>>(),
                [
                    "build",
                    "/run/pichi/repo",
                    "--cache=/run/pichi/cache",
                    "--output=/run/pichi/output",
                    "--uid-map=100000:65536",
                    "--gid-map=100000:65536",
                    "--ref=refs/heads/main",
                    "--context=project $(untouched)",
                    "--plan=a plan.yaml",
                    "--lock=a lock.yaml",
                    "--location=https://example.invalid/repo",
                ]
            );
        }
    }

    #[test]
    fn rejected_inputs_are_reported_without_starting_the_builder() {
        let (mut guest, mut host) = pair();
        let server = thread::spawn(move || {
            let _: Requests = ciborium::from_reader(&mut host).unwrap();
            ciborium::into_writer(&Response::<Input>::Err("no plan".into()), &mut host).unwrap();
            let finish: Requests = ciborium::from_reader(&mut host).unwrap();
            let Requests::Finish(Request(Finish(Some(error)))) = finish else {
                panic!("expected failed finish")
            };
            assert!(error.contains("no plan"));
            ciborium::into_writer(&Response::Ok(None::<()>), &mut host).unwrap();
        });
        let error = guest.run(Command::new("must-not-execute")).unwrap_err();
        assert!(format!("{error:#}").contains("no plan"));
        server.join().unwrap();
    }

    #[test]
    fn response_rejects_truncation_and_trailing_bytes() {
        let mut exact = Vec::new();
        ciborium::into_writer(&Response::Ok(input()), &mut exact).unwrap();
        for bytes in [exact[..exact.len() - 1].to_vec(), [exact, vec![0]].concat()] {
            let (mut guest, mut host) = pair();
            host.write_all(&bytes).unwrap();
            drop(host);
            assert!(guest.response::<Input>().is_err());
        }
    }

    #[test]
    fn response_rejects_trailing_bytes_after_a_full_prefetch() {
        let mut value = input();
        value.r#ref.clear();
        let mut bytes = Vec::new();
        ciborium::into_writer(&Response::Ok(&value), &mut bytes).unwrap();
        let capacity = BufReader::new(&[][..]).capacity();
        // The longer CBOR string needs two additional length bytes.
        value.r#ref = "x".repeat(capacity - bytes.len() - 2);
        bytes.clear();
        ciborium::into_writer(&Response::Ok(&value), &mut bytes).unwrap();
        assert_eq!(bytes.len(), capacity);
        bytes.push(0);
        let (mut guest, mut host) = pair();
        host.write_all(&bytes).unwrap();
        assert!(guest.response::<Input>().is_err());
    }

    #[test]
    fn builder_failure_survives_a_disconnected_host() {
        let (mut guest, mut host) = pair();
        let server = thread::spawn(move || {
            let _: Requests = ciborium::from_reader(&mut host).unwrap();
            ciborium::into_writer(&Response::Ok(input()), &mut host).unwrap();
        });
        let mut builder = Command::new("sh");
        builder.args(["-c", "exit 7"]);
        let error = guest.run(builder).unwrap_err();
        assert!(format!("{error:#}").contains("exit status: 7"));
        server.join().unwrap();
    }

    #[test]
    fn finish_requires_acknowledgement() {
        for response in [vec![], b"\x82\xf5".to_vec(), b"\x82\xf4\x62no".to_vec()] {
            let (mut guest, mut host) = pair();
            let server = thread::spawn(move || {
                let _: Requests = ciborium::from_reader(&mut host).unwrap();
                host.write_all(&response).unwrap();
            });
            assert!(guest.finish(None).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn opens_the_named_port_and_rejects_a_missing_device() {
        let root = tempfile::tempdir().unwrap();
        let sys = root.path().join("sys");
        let dev = root.path().join("dev");
        let port = sys.join("class/virtio-ports/vport0p1");
        fs::create_dir_all(&port).unwrap();
        fs::create_dir(&dev).unwrap();
        fs::write(port.join("name"), "other\n").unwrap();
        assert!(Console::open(&sys, &dev).is_err());
        fs::write(port.join("name"), format!("{PORT_NAME}\n")).unwrap();
        assert!(Console::open(&sys, &dev).is_err());
        fs::write(dev.join("vport0p1"), []).unwrap();
        Console::open(&sys, &dev)
            .unwrap()
            .0
            .write_all(b"opened")
            .unwrap();
        assert_eq!(fs::read(dev.join("vport0p1")).unwrap(), b"opened");
    }
}
