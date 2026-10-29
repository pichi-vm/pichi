// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    future::Future as _,
    io::{self, Cursor, ErrorKind, Seek as _, SeekFrom, Write as _},
    num::NonZero,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{
    Algorithm, AsyncFormat, Constraint, Format, HashType, Header, Parameters, Salt,
};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

struct FragmentedCursor {
    inner: Cursor<Vec<u8>>,
    read_size: usize,
    write_size: usize,
    pending: bool,
    seek: Option<SeekFrom>,
    fault: Option<Fault>,
    fault_after: usize,
    written: usize,
    flushes: usize,
}

#[derive(Clone, Copy)]
enum Fault {
    Write,
    WriteZero,
    SeekStart,
    SeekComplete,
    Flush,
    PendingWrite,
}

impl FragmentedCursor {
    fn new(bytes: Vec<u8>, position: u64, read_size: usize, write_size: usize) -> Self {
        let mut inner = Cursor::new(bytes);
        inner.set_position(position);
        Self {
            inner,
            read_size,
            write_size,
            pending: true,
            seek: None,
            fault: None,
            fault_after: usize::MAX,
            written: 0,
            flushes: 0,
        }
    }

    fn poll_pending(&mut self, cx: &Context<'_>) -> bool {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            true
        } else {
            self.pending = true;
            false
        }
    }

    fn position(&self) -> u64 {
        self.inner.position()
    }

    fn bytes(&self) -> &[u8] {
        self.inner.get_ref()
    }
}

impl AsyncRead for FragmentedCursor {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.poll_pending(cx) {
            return Poll::Pending;
        }

        let Ok(start) = usize::try_from(this.inner.position()) else {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::InvalidInput,
                "cursor position does not fit in usize",
            )));
        };
        let available = this.inner.get_ref().len().saturating_sub(start);
        let count = available.min(output.remaining()).min(this.read_size);
        output.put_slice(&this.inner.get_ref()[start..start + count]);
        this.inner.set_position((start + count) as u64);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for FragmentedCursor {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.poll_pending(cx) {
            return Poll::Pending;
        }
        if this.written >= this.fault_after {
            match this.fault {
                Some(Fault::Write) => return Poll::Ready(Err(ErrorKind::BrokenPipe.into())),
                Some(Fault::WriteZero) => return Poll::Ready(Ok(0)),
                Some(Fault::PendingWrite) => return Poll::Pending,
                _ => {}
            }
        }
        let mut count = input.len().min(this.write_size);
        if this.written < this.fault_after {
            count = count.min(this.fault_after - this.written);
        }
        let written = this.inner.write(&input[..count])?;
        this.written += written;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.poll_pending(cx) {
            return Poll::Pending;
        }
        if matches!(this.fault, Some(Fault::Flush)) && this.written >= this.fault_after {
            return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
        }
        this.flushes += 1;
        Poll::Ready(this.inner.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncSeek for FragmentedCursor {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        if matches!(this.fault, Some(Fault::SeekStart)) && this.written >= this.fault_after {
            return Err(ErrorKind::BrokenPipe.into());
        }
        if this.seek.is_some() {
            return Err(io::Error::other("seek already in progress"));
        }
        this.seek = Some(position);
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if this.poll_pending(cx) {
            return Poll::Pending;
        }
        if matches!(this.fault, Some(Fault::SeekComplete)) && this.written >= this.fault_after {
            return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
        }
        Poll::Ready(match this.seek.take() {
            Some(position) => this.inner.seek(position),
            None => Ok(this.inner.position()),
        })
    }
}

fn header(blocks: u64) -> Header {
    Header {
        uuid: [0x5a; 16],
        parameters: Parameters {
            hash_type: HashType::Normal,
            algorithm: Algorithm::Sha256,
            salt: Salt::new(&[1, 2, 3]).unwrap(),
            data: Geometry::<Constraint> {
                size: BlockSize::default(),
                count: NonZero::new(blocks).unwrap(),
            },
            hash: BlockSize::default(),
        },
    }
}

#[tokio::test]
async fn fragmented_pending_io_matches_synchronous_formatting() {
    const BLOCKS: u64 = 300;
    const BLOCK_SIZE: usize = 4096;
    const DATA_SIZE: usize = 300 * BLOCK_SIZE;
    const PREFIX: &[u8] = b"prefix before verity metadata";

    let mut bytes: Vec<_> = (0_u8..251).cycle().take(DATA_SIZE).collect();
    bytes.extend_from_slice(b"input after the declared extent");
    let header = header(BLOCKS);
    let total_size = header.total_size().unwrap();

    let mut sync_data = Cursor::new(bytes.clone());
    let mut sync_hash = Cursor::new(PREFIX.to_vec());
    sync_hash.set_position(PREFIX.len() as u64);
    let sync_root = Format::format(header, &mut sync_data, &mut sync_hash).unwrap();

    let mut async_data = FragmentedCursor::new(bytes, 0, 53, usize::MAX);
    let mut async_hash =
        FragmentedCursor::new(PREFIX.to_vec(), PREFIX.len() as u64, usize::MAX, 37);
    let async_root = AsyncFormat::format(header, &mut async_data, &mut async_hash)
        .await
        .unwrap();

    assert_eq!(async_root, sync_root);
    assert_eq!(async_hash.bytes(), sync_hash.get_ref());
    assert_eq!(async_data.position(), BLOCKS * 4096);
    assert_eq!(&async_hash.bytes()[..PREFIX.len()], PREFIX);
    assert_eq!(async_hash.position(), PREFIX.len() as u64 + total_size);
}

#[tokio::test]
async fn short_input_reports_unexpected_eof() {
    for length in [4096, 4096 + 17] {
        let mut data = FragmentedCursor::new(vec![0x5a; length], 0, 31, usize::MAX);
        let mut hash = FragmentedCursor::new(Vec::new(), 0, usize::MAX, 29);

        let error = AsyncFormat::format(header(2), &mut data, &mut hash)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
    }
}

#[tokio::test]
async fn oversized_extent_is_rejected_before_writing() {
    let mut data = FragmentedCursor::new(Vec::new(), 0, 17, usize::MAX);
    let mut hash = FragmentedCursor::new(Vec::new(), 0, usize::MAX, 13);

    let error = AsyncFormat::format(header(u64::MAX), &mut data, &mut hash)
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(hash.bytes(), []);
}

#[tokio::test]
async fn output_failures_do_not_return_a_root() {
    // Block 129 is read before draining the hashes of the first 128 blocks.
    for (blocks, consumed_at_first_write) in [(2, 2), (300, 129)] {
        let header = header(blocks);
        for (fault, after, expected) in [
            (Fault::Write, 4096 + 13, ErrorKind::BrokenPipe),
            (Fault::WriteZero, 4096 + 13, ErrorKind::WriteZero),
            (Fault::SeekStart, 4096, ErrorKind::BrokenPipe),
            (Fault::SeekComplete, 4096, ErrorKind::BrokenPipe),
            (Fault::SeekStart, 8192, ErrorKind::BrokenPipe),
            (Fault::SeekComplete, 8192, ErrorKind::BrokenPipe),
            (
                Fault::Flush,
                usize::try_from(header.total_size().unwrap()).unwrap(),
                ErrorKind::BrokenPipe,
            ),
        ] {
            let data_size = usize::try_from(header.parameters.data_size().unwrap()).unwrap();
            let mut data = FragmentedCursor::new(vec![0x5a; data_size], 0, 53, usize::MAX);
            let mut hash = FragmentedCursor::new(Vec::new(), 0, usize::MAX, 37);
            hash.fault = Some(fault);
            hash.fault_after = after;
            let error = AsyncFormat::format(header, &mut data, &mut hash)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), expected);
            assert_eq!(hash.written, after);
            if matches!(fault, Fault::Write | Fault::WriteZero) {
                assert_eq!(data.position(), consumed_at_first_write * 4096);
            }
        }
    }
}

#[test]
fn cancellation_leaves_partial_output_without_finishing_it() {
    // The larger tree reserves its first hash block for the upper level.
    for (blocks, consumed_at_first_write, first_tree_offset) in [(2, 2, 4096), (300, 129, 8192)] {
        let mut data = FragmentedCursor::new(
            vec![0x5a; usize::try_from(blocks * 4096).unwrap()],
            0,
            usize::MAX,
            usize::MAX,
        );
        let mut hash = FragmentedCursor::new(Vec::new(), 0, usize::MAX, usize::MAX);
        hash.fault = Some(Fault::PendingWrite);
        hash.fault_after = 4096 + 13;
        let mut formatting = Box::pin(AsyncFormat::format(header(blocks), &mut data, &mut hash));
        let mut context = Context::from_waker(Waker::noop());
        // Advance past the finite fragmented-I/O steps to the stalled tree write.
        for _ in 0..1000 {
            assert!(formatting.as_mut().poll(&mut context).is_pending());
        }
        drop(formatting);

        assert_eq!(data.position(), consumed_at_first_write * 4096);
        assert_eq!(hash.written, 4096 + 13);
        assert_eq!(hash.bytes().len(), first_tree_offset + 13);
        // The padding copy flushes once; cancellation must not perform a final flush.
        assert_eq!(hash.flushes, 1);
    }
}

#[tokio::test]
async fn single_block_has_no_tree_and_preserves_trailing_input() {
    for exponent in [9, 12, 14] {
        let mut header = header(1);
        header.parameters.data.size = BlockSize::from_exponent(exponent).unwrap();
        let block_size = header.parameters.data.size.bytes().get() as usize;
        let bytes = vec![0x5a; block_size + 17];
        let mut expected = Cursor::new(Vec::new());
        let root = Format::format(header, Cursor::new(&bytes), &mut expected).unwrap();
        let mut data = FragmentedCursor::new(bytes, 0, 53, usize::MAX);
        let mut hash = FragmentedCursor::new(Vec::new(), 0, usize::MAX, 37);
        assert_eq!(
            AsyncFormat::format(header, &mut data, &mut hash)
                .await
                .unwrap(),
            root
        );
        assert_eq!(data.position(), block_size as u64);
        assert_eq!(hash.bytes(), expected.get_ref());
        assert_eq!(hash.position(), header.total_size().unwrap());
    }
}
