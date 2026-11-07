// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Bounded client framing using pichi's existing async runtime.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::{
    GenericFilePath,
    tokio::{Stream, prelude::*},
};
use madrillo::{ClientMessage, PROTOCOL_VERSION, Request, Response, ServerMessage};
use std::{path::Path, time::Duration};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Client(Framed<Stream, LengthDelimitedCodec>);

impl Client {
    pub(super) async fn connect(path: &Path) -> Result<Self> {
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            Stream::connect(path.to_fs_name::<GenericFilePath>()?),
        )
        .await
        .context("monitor connect deadline")??;
        #[cfg(unix)]
        if stream.peer_creds()?.euid() != Some(nix::unistd::geteuid().as_raw()) {
            bail!("monitor endpoint belongs to another account");
        }
        #[cfg(windows)]
        {
            use win_custody::{ObjectSecurityDescriptor, OwnedSid};
            let Stream::NamedPipe(pipe) = &stream;
            let descriptor = ObjectSecurityDescriptor::query_kernel_object(pipe.inner())?;
            let owner = descriptor.owner()?;
            let user = OwnedSid::from_string(&win_custody::current_user_sid_string()?)?;
            let system = OwnedSid::from_string("S-1-5-18")?;
            let administrators = OwnedSid::from_string("S-1-5-32-544")?;
            if ![user, system, administrators]
                .iter()
                .any(|sid| owner.matches(sid))
            {
                bail!("monitor endpoint belongs to another account");
            }
        }
        Ok(Self::new(stream))
    }

    pub(super) fn new(stream: Stream) -> Self {
        Self(
            LengthDelimitedCodec::builder()
                .max_frame_length(MAX_FRAME)
                .new_framed(stream),
        )
    }

    pub(super) async fn send(&mut self, message: ClientMessage) -> Result<()> {
        let frame = serde_json::to_vec(&Request {
            version: PROTOCOL_VERSION,
            message,
        })?;
        if frame.len() > MAX_FRAME {
            bail!("monitor request exceeds frame limit");
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            self.0.send(bytes::Bytes::from(frame)),
        )
        .await
        .context("monitor control write deadline")??;
        Ok(())
    }

    // Framed retains partially received bytes when a select branch is cancelled.
    pub(super) async fn next(&mut self) -> Result<ServerMessage> {
        let frame = self
            .0
            .next()
            .await
            .context("monitor disconnected without completion")??;
        let response: Response = serde_json::from_slice(&frame)?;
        if response.version != PROTOCOL_VERSION {
            bail!("unsupported monitor protocol {}", response.version);
        }
        Ok(response.message)
    }
}

#[cfg(windows)]
impl Drop for Client {
    fn drop(&mut self) {
        let Stream::NamedPipe(pipe) = self.0.get_ref();
        pipe.inner().assume_flushed();
    }
}
