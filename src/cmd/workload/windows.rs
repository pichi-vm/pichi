// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Private first-hop Windows inheritance. Madrillo alone owns workload Jobs.
//! No child arguments are interpreted and dropping Monitor only detaches.

use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation},
    Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal},
    System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
        CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    },
};

#[derive(Debug)]
pub(super) struct Monitor {
    // Owning the native handle keeps identity stable through the final wait.
    handle: OwnedHandle,
    id: u32,
}

impl Monitor {
    pub(super) fn spawn(
        program: &Path,
        arguments: &[OsString],
        resources: &[BorrowedHandle<'_>],
        diagnostic: File,
    ) -> io::Result<Self> {
        let mut command = NativeText::command(program, arguments)?;
        let environment = NativeText::environment()?;
        let input = File::open("NUL")?;
        let output = File::options().write(true).open("NUL")?;
        let stdio = [&input, &output, &diagnostic];
        let handles: Vec<_> = stdio
            .iter()
            .map(|f| f.as_handle())
            .chain(resources.iter().copied())
            .collect();
        let inheritance = Inheritance::new(&handles)?;
        let native: Vec<_> = handles.iter().map(AsRawHandle::as_raw_handle).collect();
        // The list owns its backing until after CreateProcessW returns.
        let mut attributes = Attributes::new(&native)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = input.as_raw_handle();
        startup.StartupInfo.hStdOutput = output.as_raw_handle();
        startup.StartupInfo.hStdError = diagnostic.as_raw_handle();
        startup.lpAttributeList = attributes.pointer();
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: all UTF-16 buffers are NUL-terminated; command is mutable.
        // Attribute backing and borrowed handles outlive the call. The explicit
        // list includes actual stdio handles, not duplicates made by std::Command.
        let created = unsafe {
            CreateProcessW(
                null(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT
                    | DETACHED_PROCESS
                    | CREATE_NEW_PROCESS_GROUP
                    | CREATE_BREAKAWAY_FROM_JOB,
                environment.as_ptr().cast(),
                null(),
                &raw const startup.StartupInfo,
                &raw mut process,
            )
        };
        // Read the creation error before guard cleanup can change LastError.
        let error = (created == 0).then(io::Error::last_os_error);
        drop(attributes);
        drop(inheritance);
        if let Some(error) = error {
            return Err(error);
        }
        // SAFETY: successful creation transfers unique process/thread handles.
        let handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
        drop(unsafe { OwnedHandle::from_raw_handle(process.hThread) });
        Ok(Self {
            handle,
            id: process.dwProcessId,
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.id
    }

    pub(super) fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.observe(0)
    }

    #[cfg(test)]
    pub(super) fn wait(&self) -> io::Result<std::process::ExitStatus> {
        self.observe(windows_sys::Win32::System::Threading::INFINITE)?
            .ok_or_else(|| io::Error::other("infinite monitor wait timed out"))
    }

    fn observe(&self, timeout: u32) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::windows::process::ExitStatusExt;
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{GetExitCodeProcess, WaitForSingleObject},
        };
        // SAFETY: the owned process handle remains valid through wait/query.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), timeout) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: signaled process and valid writable output. 259 is a
                // real exit code here, never a proxy for process liveness.
                if unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &raw mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(std::process::ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    #[cfg(test)]
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: the handle names this test-owned monitor, not a recycled PID.
        if unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(self.handle.as_raw_handle(), 1)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

struct Inheritance<'a> {
    handles: &'a [BorrowedHandle<'a>],
    flags: Vec<u32>,
}
impl<'a> Inheritance<'a> {
    fn new(handles: &'a [BorrowedHandle<'a>]) -> io::Result<Self> {
        let mut guard = Self {
            handles,
            flags: Vec::new(),
        };
        for handle in handles {
            let mut flags = 0;
            // SAFETY: each borrowed handle stays open for the guard lifetime.
            if unsafe { GetHandleInformation(handle.as_raw_handle(), &raw mut flags) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: only the inheritance flag changes, restored on every exit.
            if unsafe {
                SetHandleInformation(
                    handle.as_raw_handle(),
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            guard.flags.push(flags);
        }
        Ok(guard)
    }
}
impl Drop for Inheritance<'_> {
    fn drop(&mut self) {
        for (handle, flags) in self.handles.iter().zip(&self.flags) {
            // SAFETY: all borrowed handles outlive this restoration.
            unsafe {
                SetHandleInformation(
                    handle.as_raw_handle(),
                    HANDLE_FLAG_INHERIT,
                    *flags & HANDLE_FLAG_INHERIT,
                );
            }
        }
    }
}

struct Attributes<'a> {
    storage: Vec<u128>,
    _handles: &'a [windows_sys::Win32::Foundation::HANDLE],
}
impl<'a> Attributes<'a> {
    fn new(handles: &'a [windows_sys::Win32::Foundation::HANDLE]) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: null-list size query with valid output storage.
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &raw mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = Self {
            storage: vec![0; bytes.div_ceil(size_of::<u128>())],
            _handles: handles,
        };
        // SAFETY: 16-byte aligned storage covers the API's queried size.
        if unsafe { InitializeProcThreadAttributeList(attributes.pointer(), 1, 0, &raw mut bytes) }
            == 0
        {
            let error = io::Error::last_os_error();
            attributes.storage.clear();
            return Err(error);
        }
        // SAFETY: immutable backing is borrowed for the list's entire lifetime;
        // caller owns the live inheritable handles through process creation.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        if !self.storage.is_empty() {
            // SAFETY: the initialized list and backing remain alive until here.
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }
}

struct NativeText;
impl NativeText {
    fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut value: Vec<_> = value.encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native text contains NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }
    fn command(program: &Path, arguments: &[OsString]) -> io::Result<Vec<u16>> {
        let program = program.as_os_str();
        if program.is_empty() || program.encode_wide().any(|c| c == 34) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid monitor executable",
            ));
        }
        let mut output = Vec::new();
        for text in std::iter::once(program).chain(arguments.iter().map(OsString::as_os_str)) {
            if !output.is_empty() {
                output.push(32);
            }
            output.push(34);
            let value = Self::wide(text)?;
            let mut slashes = 0;
            for c in &value[..value.len() - 1] {
                if *c == 92 {
                    slashes += 1;
                    continue;
                }
                output.extend(std::iter::repeat_n(
                    92,
                    if *c == 34 { slashes * 2 + 1 } else { slashes },
                ));
                output.push(*c);
                slashes = 0;
            }
            output.extend(std::iter::repeat_n(92, slashes * 2));
            output.push(34);
        }
        output.push(0);
        if output.len() > 32767 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows command line too long",
            ));
        }
        Ok(output)
    }
    fn compare(a: &[u16], b: &[u16]) -> io::Result<Ordering> {
        // SAFETY: borrowed UTF-16 slices remain valid; lengths fit the API.
        let result = unsafe {
            CompareStringOrdinal(
                a.as_ptr(),
                i32::try_from(a.len()).map_err(io::Error::other)?,
                b.as_ptr(),
                i32::try_from(b.len()).map_err(io::Error::other)?,
                1,
            )
        };
        match result {
            CSTR_LESS_THAN => Ok(Ordering::Less),
            CSTR_EQUAL => Ok(Ordering::Equal),
            CSTR_GREATER_THAN => Ok(Ordering::Greater),
            _ => Err(io::Error::last_os_error()),
        }
    }
    fn environment() -> io::Result<Vec<u16>> {
        let mut entries = std::env::vars_os().collect::<Vec<_>>();
        if std::env::var_os("RUST_LOG").is_none() {
            entries.push(("RUST_LOG".into(), "warn".into()));
        }
        let mut entries = entries
            .into_iter()
            .map(|(key, value)| {
                let native_key = Self::wide(&key)?;
                let mut text = key;
                text.push("=");
                text.push(value);
                Ok((native_key, Self::wide(&text)?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut error = None;
        entries.sort_by(|a, b| {
            Self::compare(&a.0, &b.0).unwrap_or_else(|e| {
                error = Some(e);
                Ordering::Equal
            })
        });
        if let Some(error) = error {
            return Err(error);
        }
        let mut block = entries
            .into_iter()
            .flat_map(|(_, value)| value)
            .collect::<Vec<_>>();
        block.push(0);
        Ok(block)
    }
}
