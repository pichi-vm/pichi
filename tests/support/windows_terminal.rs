// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Actual Pichi CLI inside a native ConPTY, outside restrictive runner Jobs.

#![forbid(unsafe_code)]

use super::{graphroot, make_stub_dillo, populate_bootable};
use interprocess::local_socket::{
    GenericFilePath, ListenerNonblockingMode, ListenerOptions, prelude::*,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wait_timeout::ChildExt as _;

struct Workload {
    root: tempfile::TempDir,
    stub: PathBuf,
}

impl Workload {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        populate_bootable(&graphroot(&root), "terminal:1").await;
        let stub = make_stub_dillo(root.path());
        Self { root, stub }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pichi"));
        command
            .env("XDG_DATA_HOME", self.root.path())
            .env(
                "PATH",
                super::executable_path::prepend(self.stub.parent().unwrap()),
            )
            .env(
                "PICHI_TEST_TERMINAL_FIXTURE",
                std::env::current_exe().unwrap(),
            );
        command
    }

    fn id(&self) -> String {
        let records: Vec<_> = std::fs::read_dir(self.root.path().join("pichi/storage.workloads"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        records[0].clone()
    }

    fn run(&self, arguments: &[&str]) -> Vec<u8> {
        let output = assert_cmd::Command::from_std(self.command())
            .args(arguments)
            .timeout(Duration::from_secs(10))
            .assert()
            .success();
        output.get_output().stdout.clone()
    }
}

impl Drop for Workload {
    fn drop(&mut self) {
        let Ok(records) = std::fs::read_dir(self.root.path().join("pichi/storage.workloads"))
        else {
            return;
        };
        for entry in records.flatten() {
            let id = entry.file_name();
            let mut command = self.command();
            command
                .args(["stop", "--time", "0"])
                .arg(id)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(mut child) = command.spawn() {
                if !matches!(child.wait_timeout(Duration::from_secs(5)), Ok(Some(_))) {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }
}

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    slave: Box<dyn portable_pty::SlavePty + Send>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<io::Result<Vec<u8>>>,
    received: Vec<u8>,
}

impl Terminal {
    fn start(workload: &Workload, arguments: &[&str], cols: u16, rows: u16) -> Self {
        // This fixture hosts the CLI only. Unlike the monitor's workload Job,
        // it must permit Pichi to launch its independent Madrillo process.
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                cols,
                rows,
                ..PtySize::default()
            })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (send, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut total = 0;
            loop {
                let mut bytes = vec![0; 4096];
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        total += count;
                        if total > 1024 * 1024 {
                            break;
                        }
                        bytes.truncate(count);
                        if send.send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = send.send(Err(error));
                        break;
                    }
                }
            }
        });
        let writer = pair.master.take_writer().unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pichi"));
        command.args(arguments);
        command.env("XDG_DATA_HOME", workload.root.path());
        command.env(
            "PATH",
            super::executable_path::prepend(workload.stub.parent().unwrap()),
        );
        command.env(
            "PICHI_TEST_TERMINAL_FIXTURE",
            std::env::current_exe().unwrap(),
        );
        let child = pair.slave.spawn_command(command).unwrap();
        Self {
            child,
            master: pair.master,
            slave: pair.slave,
            writer,
            output,
            received: Vec::new(),
        }
    }

    fn expect(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            // ConPTY requests its host's cursor position after startup. A
            // response written before the request can be discarded as input.
            while let Some(index) = self.received.windows(4).position(|part| part == b"\x1b[6n") {
                self.received.drain(index..index + 4);
                self.writer.write_all(b"\x1b[1;1R").unwrap();
            }
            if let Some(index) = self
                .received
                .windows(marker.len())
                .position(|part| part == marker.as_bytes())
            {
                self.received.drain(..index + marker.len());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {marker}: {}",
                String::from_utf8_lossy(&self.received)
            );
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => self.received.extend(bytes.unwrap()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!(
                    "terminal ended before {marker}: {error}; {}",
                    String::from_utf8_lossy(&self.received)
                ),
            }
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                cols,
                rows,
                ..PtySize::default()
            })
            .unwrap();
        self.expect(&format!("<<<SIZE:{cols}x{rows}>>>"));
    }

    fn input(&mut self) {
        self.writer.write_all(b"input-\x03\x04!").unwrap();
        self.expect("<<<INPUT:696e7075742d030421>>>");
    }

    fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "Pichi terminal exit: {status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "Pichi terminal did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn detach(&mut self) {
        self.writer.write_all(&[16, 17]).unwrap();
        self.wait();
        self.restored();
    }

    fn restored(&mut self) {
        // Reuse the same console to observe the native input mode left by
        // Pichi, rather than checking a new console's default mode.
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "windows_terminal::restored",
            "--exact",
            "--ignored",
            "--nocapture",
        ]);
        self.child = self.slave.spawn_command(command).unwrap();
        self.expect("<<<TERMINAL-RESTORED>>>");
        self.wait();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
#[ignore = "requires real Madrillo and execution outside restrictive runner/Cargo Jobs"]
async fn interactive_resize_input_and_reattach() {
    let workload = Workload::new().await;
    let mut terminal = Terminal::start(&workload, &["run", "-i", "terminal:1"], 300, 77);
    terminal.expect("<<<RESIZE-READY>>>");
    terminal.expect("<<<SIZE:300x77>>>");
    terminal.input();
    terminal.resize(120, 41);
    let id = workload.id();
    terminal.detach();
    drop(terminal);
    let mut attached = Terminal::start(&workload, &["attach", "-i", &id], 81, 25);
    attached.expect("<<<SIZE:81x25>>>");
    attached.input();
    attached.detach();
    drop(attached);
    workload.run(&["stop", &id]);
    workload.run(&["wait", &id]);
    drop(workload);

    let workload = Workload::new().await;
    let id = String::from_utf8(workload.run(&["run", "-d", "terminal:1"])).unwrap();
    let id = id.trim();
    let mut attached = Terminal::start(&workload, &["attach", "-i", id], 132, 43);
    attached.expect("<<<SIZE:132x43>>>");
    attached.input();
    workload.run(&["stop", id]);
    attached.wait();
    attached.restored();
    workload.run(&["wait", id]);
    println!(
        "native Windows CLI terminal: run, input, resize, detach, reattach, detached start and stop passed"
    );
}

#[test]
#[ignore = "internal console mode probe: invoked in the same terminal after Pichi exits"]
fn restored() {
    use std::io::IsTerminal as _;
    assert!(io::stdin().is_terminal());
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    println!("<<<TERMINAL-RESTORED>>>");
}

#[test]
#[ignore = "internal Dillo fixture: only the native terminal test invokes this"]
fn workload() {
    let path = PathBuf::from(std::env::var_os("PICHI_TEST_RESIZE_ENDPOINT").unwrap());
    let listener = ListenerOptions::new()
        .name(path.to_fs_name::<GenericFilePath>().unwrap())
        .nonblocking(ListenerNonblockingMode::Accept)
        .create_sync()
        .unwrap();
    println!("<<<RESIZE-READY>>>");
    io::stdout().flush().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let finish = done.clone();
    let updates = std::thread::spawn(move || {
        while !finish.load(Ordering::Acquire) {
            match listener.accept() {
                Ok(mut stream) => {
                    eprintln!("resize fixture accepted a connection");
                    let mut bytes = [0; 4];
                    stream.read_exact(&mut bytes).unwrap();
                    let size = dillo_config::ConsoleSize::try_from(bytes).unwrap();
                    println!("<<<SIZE:{}x{}>>>", size.cols, size.rows);
                    io::stdout().flush().unwrap();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("resize listener failed: {error}"),
            }
        }
    });
    let mut input = Vec::new();
    let mut bytes = [0; 256];
    loop {
        let count = io::stdin().read(&mut bytes).unwrap();
        if count == 0 {
            break;
        }
        for &byte in &bytes[..count] {
            input.push(byte);
            assert!(input.len() < 4096);
            if byte == b'!' {
                println!("<<<INPUT:{}>>>", hex::encode(&input));
                io::stdout().flush().unwrap();
                input.clear();
            }
        }
    }
    done.store(true, Ordering::Release);
    updates.join().unwrap();
}
