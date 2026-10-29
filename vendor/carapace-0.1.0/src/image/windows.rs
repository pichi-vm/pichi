// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::File,
    io,
    mem::size_of,
    ops::Range,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_FUNCTION, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NOT_SUPPORTED},
    System::{
        Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES},
        Threading::CreateEventW,
        IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED},
    },
};

use super::SparseFile;

impl SparseFile for File {
    fn next_extent(&self, position: u64, size: u64) -> io::Result<Range<u64>> {
        if position >= size {
            return Ok(size..size);
        }
        let request = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: i64::try_from(position)
                .map_err(|_| super::invalid("file offset overflow"))?,
            Length: i64::try_from(size - position)
                .map_err(|_| super::invalid("file length overflow"))?,
        };
        let mut range = FILE_ALLOCATED_RANGE_BUFFER::default();
        let bytes = match query(self, &request, &mut range) {
            Ok(bytes) => bytes,
            Err(e)
                if matches!(
                    e.raw_os_error().map(|code| code as u32),
                    Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED)
                ) =>
            {
                return Ok(position..size);
            }
            Err(e) => return Err(e),
        };
        if bytes == 0 {
            return Ok(size..size);
        }
        if bytes as usize != size_of::<FILE_ALLOCATED_RANGE_BUFFER>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid allocated-range response size",
            ));
        }
        clip(range, position..size)
    }
}

fn clip(range: FILE_ALLOCATED_RANGE_BUFFER, requested: Range<u64>) -> io::Result<Range<u64>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid allocated range");
    let start = u64::try_from(range.FileOffset).map_err(|_| invalid())?;
    let length = u64::try_from(range.Length).map_err(|_| invalid())?;
    let end = start.checked_add(length).ok_or_else(invalid)?;
    // NTFS may round the query outwards. Only scan the requested interval.
    let start = start.max(requested.start);
    let end = end.min(requested.end);
    if start >= end {
        return Err(invalid());
    }
    Ok(start..end)
}

// One record is enough: the scanner resumes at its end, so ERROR_MORE_DATA
// does not require retaining the entire file's extent list in memory.
#[expect(unsafe_code, reason = "private Windows allocated-range FFI boundary")]
fn query(
    file: &File,
    request: &FILE_ALLOCATED_RANGE_BUFFER,
    range: &mut FILE_ALLOCATED_RANGE_BUFFER,
) -> io::Result<u32> {
    // SAFETY: All pointers refer to initialized, correctly sized values that
    // remain live until the operation completes. The file is borrowed throughout.
    // A private event supports both synchronous and overlapped file handles;
    // pending I/O is waited for before any stack storage or handle is released.
    // Setting the event handle's low bit suppresses IOCP notifications, preventing
    // a caller's completion port from receiving a pointer to our stack OVERLAPPED.
    // The event's unmodified handle is owned exactly once and closed by RAII.
    unsafe {
        // Test-only setup stays within this one audited FFI boundary. The test
        // supplies a fresh overlapped handle; the new port is owned exactly once.
        #[cfg(test)]
        let completion_port = if CHECK_IOCP.replace(false) {
            let port = windows_sys::Win32::System::IO::CreateIoCompletionPort(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                1,
            );
            assert!(!port.is_null(), "{}", io::Error::last_os_error());
            Some(OwnedHandle::from_raw_handle(port))
        } else {
            None
        };
        let event = CreateEventW(null(), 1, 0, null());
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = OwnedHandle::from_raw_handle(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle().map_addr(|address| address | 1),
            ..OVERLAPPED::default()
        };
        let mut bytes = 0;
        let result = DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_QUERY_ALLOCATED_RANGES,
            std::ptr::from_ref(request).cast(),
            size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
            std::ptr::from_mut(range).cast(),
            size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
            &raw mut bytes,
            &raw mut overlapped,
        );
        let initial = if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        let completed = finish_query(initial, &mut bytes, |bytes| {
            if GetOverlappedResult(file.as_raw_handle(), &raw const overlapped, bytes, 1) == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
        #[cfg(test)]
        if let Some(port) = completion_port {
            let mut transferred = 0;
            let mut key = 0;
            let mut packet = std::ptr::null_mut();
            let received = windows_sys::Win32::System::IO::GetQueuedCompletionStatus(
                port.as_raw_handle(),
                &raw mut transferred,
                &raw mut key,
                &raw mut packet,
                100,
            );
            let error = io::Error::last_os_error();
            assert_eq!(received, 0, "query posted an IOCP notification");
            assert!(packet.is_null(), "query posted a failed IOCP notification");
            assert_eq!(
                error.raw_os_error(),
                Some(windows_sys::Win32::Foundation::WAIT_TIMEOUT.cast_signed())
            );
        }
        completed
    }
}

// Keep completion/error handling separate from FFI so every pending outcome can
// be tested deterministically, even when a local filesystem completes inline.
fn finish_query(
    initial: io::Result<()>,
    bytes: &mut u32,
    wait: impl FnOnce(&mut u32) -> io::Result<()>,
) -> io::Result<u32> {
    let completed = match initial {
        Err(e) if e.raw_os_error() == Some(ERROR_IO_PENDING.cast_signed()) => wait(bytes),
        other => other,
    };
    if let Err(error) = completed {
        if error.raw_os_error() != Some(ERROR_MORE_DATA.cast_signed()) {
            return Err(error);
        }
        if *bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query made no progress",
            ));
        }
    }
    Ok(*bytes)
}

#[cfg(test)]
std::thread_local! {
    // Only the next query on this test thread attaches a completion port.
    static CHECK_IOCP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn pending_completion_waits_and_preserves_partial_results_and_errors() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_OPERATION_ABORTED};
        let pending = || Err(io::Error::from_raw_os_error(ERROR_IO_PENDING.cast_signed()));
        for status in [
            None,
            Some(ERROR_MORE_DATA),
            Some(ERROR_ACCESS_DENIED),
            Some(ERROR_OPERATION_ABORTED),
        ] {
            let mut waited = false;
            let mut bytes = 0;
            let result = finish_query(pending(), &mut bytes, |bytes| {
                waited = true;
                *bytes = 16;
                status.map_or(Ok(()), |code| {
                    Err(io::Error::from_raw_os_error(code.cast_signed()))
                })
            });
            assert!(waited);
            match status {
                None | Some(ERROR_MORE_DATA) => assert_eq!(result.unwrap(), 16),
                Some(code) => {
                    assert_eq!(result.unwrap_err().raw_os_error(), Some(code.cast_signed()));
                }
            }
        }
        let result = finish_query(pending(), &mut 0, |_| {
            Err(io::Error::from_raw_os_error(ERROR_MORE_DATA.cast_signed()))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn immediate_completion_does_not_wait() {
        assert_eq!(
            finish_query(Ok(()), &mut 16, |_| panic!("unexpected wait")).unwrap(),
            16
        );
        let partial = || Err(io::Error::from_raw_os_error(ERROR_MORE_DATA.cast_signed()));
        assert_eq!(
            finish_query(partial(), &mut 16, |_| panic!("unexpected wait")).unwrap(),
            16
        );
        assert_eq!(
            finish_query(partial(), &mut 0, |_| panic!("unexpected wait"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let unsupported = Err(io::Error::from_raw_os_error(
            ERROR_NOT_SUPPORTED.cast_signed(),
        ));
        assert_eq!(
            finish_query(unsupported, &mut 0, |_| panic!("unexpected wait"))
                .unwrap_err()
                .raw_os_error(),
            Some(ERROR_NOT_SUPPORTED.cast_signed())
        );
    }

    #[test]
    fn iocp_query_does_not_post_completion_packets() {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(&[1; 8192]).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(source.path())
            .unwrap();
        CHECK_IOCP.set(true);
        assert_eq!(file.next_extent(0, 8192).unwrap(), 0..8192);
        assert!(!CHECK_IOCP.get(), "query did not exercise IOCP check");
    }

    #[test]
    fn clips_rounded_ranges_and_rejects_invalid_responses() {
        let range = |offset, length| FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: offset,
            Length: length,
        };
        assert_eq!(clip(range(0, 16384), 4096..8192).unwrap(), 4096..8192);
        for invalid in [
            range(-1, 1),
            range(0, -1),
            range(0, 0),
            range(0, 4096),
            range(8192, 1),
        ] {
            assert!(clip(invalid, 4096..8192).is_err());
        }
    }

    #[test]
    fn queries_regular_file_and_eof() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[1; 8192]).unwrap();
        assert_eq!(file.next_extent(0, 8192).unwrap(), 0..8192);
        assert_eq!(file.next_extent(4096, 8192).unwrap(), 4096..8192);
        assert_eq!(file.next_extent(8192, 8192).unwrap(), 8192..8192);
    }

    #[test]
    fn sparse_queries_resume_after_partial_results_for_both_handle_modes() {
        use super::super::{Plan as _, Planned};
        use std::{
            fs::OpenOptions,
            io::{Seek, SeekFrom},
            os::windows::fs::OpenOptionsExt,
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        const SIZE: u64 = 16 * 1024 * 1024;
        const SECOND: u64 = SIZE / 2;
        let mut source = tempfile::NamedTempFile::new().unwrap();
        // Native Windows fixture setup; production conversion never runs a tool.
        // Keep this outside the FFI boundary so tests add no unsafe exceptions.
        let result = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(source.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "fsutil: {result:?}");
        source.as_file().set_len(SIZE).unwrap();
        assert_eq!(source.as_file().next_extent(0, SIZE).unwrap(), SIZE..SIZE);
        source.write_all(&[1; 4096]).unwrap();
        source.seek(SeekFrom::Start(SECOND)).unwrap();
        source.write_all(&[2; 4096]).unwrap();
        source.flush().unwrap();

        for flags in [0, FILE_FLAG_OVERLAPPED] {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(flags)
                .open(source.path())
                .unwrap();
            // A one-record output buffer must return the first extent even
            // when the query reports ERROR_MORE_DATA for the second extent.
            // Also verify suppression when the operation returns partial data.
            CHECK_IOCP.set(flags == FILE_FLAG_OVERLAPPED);
            let first = file.next_extent(0, SIZE).unwrap();
            assert!(!CHECK_IOCP.get());
            assert_eq!(first.start, 0);
            assert!((4096..SECOND).contains(&first.end));
            let second = file.next_extent(first.end, SIZE).unwrap();
            assert!(second.start <= SECOND && second.start >= first.end);
            assert!(second.end >= SECOND + 4096 && second.end < SIZE);
            assert_eq!(file.next_extent(second.end, SIZE).unwrap(), SIZE..SIZE);
        }
        let planned = Planned::plan(File::open(source.path()).unwrap()).unwrap();
        assert_eq!(planned.exceptions(), 2);
        assert!(planned.metadata.read_bytes < SIZE / 2);
        #[cfg(feature = "tokio")]
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                use super::super::AsyncPlan as _;
                let file = tokio::fs::File::open(source.path()).await.unwrap();
                let asynchronous = Planned::<tokio::fs::File>::plan(file).await.unwrap();
                assert_eq!(asynchronous.exceptions(), planned.exceptions());
                assert_eq!(asynchronous.size(), planned.size());
                assert!(asynchronous.metadata.read_bytes < SIZE / 2);
            });
    }
}
