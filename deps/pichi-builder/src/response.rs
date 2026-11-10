// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure};
use pichi_launch::{Input, Response};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
};

fn decode(reader: &mut impl Read, capacity: usize, refill: bool) -> anyhow::Result<Input> {
    let mut buffered = BufReader::with_capacity(capacity, reader);
    let response: Response<Input> = ciborium::from_reader(&mut buffered)?;
    ensure!(buffered.buffer().is_empty(), "trailing build input bytes");
    if refill {
        ensure!(
            buffered.fill_buf()?.is_empty(),
            "trailing build input bytes"
        );
    }
    match response {
        Response::Ok(input) => Ok(input),
        Response::Err(error) => bail!("host rejected build: {error}"),
    }
}

pub(crate) fn read_port(port: &mut File) -> anyhow::Result<Input> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let input = decode(port, 8192, false)?;
    // Do not block refilling a port after its complete self-delimiting response.
    let mut fds = [PollFd::new(&*port, PollFlags::IN)];
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
        "build-port poll failed"
    );
    if events.intersects(PollFlags::IN | PollFlags::HUP) {
        let mut byte = [0];
        match port.read(&mut byte) {
            Ok(0) => {}
            Ok(_) => bail!("trailing build input bytes"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_input_roundtrip_rejects_trailing_bytes_at_buffer_boundary() {
        let input = Input {
            r#ref: "abc".into(),
            plan: "plan.yaml".into(),
            lock: "lock.yaml".into(),
            context: ".".into(),
            location: None,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&Response::Ok(input.clone()), &mut bytes).unwrap();
        for capacity in [1, bytes.len(), bytes.len() + 1] {
            assert_eq!(
                decode(&mut bytes.as_slice(), capacity, true).unwrap(),
                input
            );
            let mut trailing = bytes.clone();
            trailing.push(0);
            assert!(decode(&mut trailing.as_slice(), capacity, true).is_err());
        }
        let mut error = Vec::new();
        ciborium::into_writer(
            &Response::<Input>::Err("input unavailable".into()),
            &mut error,
        )
        .unwrap();
        assert!(
            decode(&mut error.as_slice(), 8192, true)
                .unwrap_err()
                .to_string()
                .contains("input unavailable")
        );
    }
}
