// SPDX-License-Identifier: Apache-2.0

use std::{
    fmt::Debug,
    fs::{File, OpenOptions},
    io,
    os::fd::{AsFd, BorrowedFd},
    sync::Arc,
};

use crate::{
    DevId, Status, Table,
    abi::{Command, Header},
    records::{self, Dependencies, Devices, Records, Row, Rows, Versions},
    targets::Version,
};

/// A connection for creating, inspecting, and managing device-mapper mappings.
///
/// Clones share the same file descriptor. Operations are synchronous and address
/// devices by [`DevId`]; a sequence of calls is not a transaction.
///
/// # Device lifetime
///
/// Creating, loading, and activating a mapping are separate operations. Dropping
/// a connection does not remove devices. Callers must arrange cleanup, including
/// when setup fails. See [`create`](Self::create), [`load`](Self::load),
/// [`suspend`](Self::suspend), and [`remove`](Self::remove).
///
/// # Query buffers
///
/// Variable-length queries take a capacity `N` in bytes, excluding the ioctl
/// header. Replies own inline storage and can outlive this connection; larger
/// capacities increase their size. On [`StorageFull`](io::ErrorKind::StorageFull),
/// issue another query with a larger const-generic `N`. Traverse replies with
/// [`Records`].
///
/// # Errors
///
/// Operating-system errors are propagated, including permission failures,
/// missing devices, and unsupported kernel operations. Individual methods
/// document input validation and reply-decoding errors.
#[derive(Clone, Debug)]
pub struct Control(Arc<File>);

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Control>();
};

impl Control {
    /// Opens the device-mapper control file.
    ///
    /// Opening `/dev/mapper/control` requires read/write permission. Executing
    /// device-mapper operations normally also requires `CAP_SYS_ADMIN`; successfully
    /// opening the file does not establish that later operations will succeed.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the control file cannot be opened.
    pub fn open() -> io::Result<Self> {
        Ok(Self(Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/mapper/control")?,
        )))
    }

    #[cfg(test)]
    pub(crate) fn dummy() -> Self {
        Self(Arc::new(File::open("/dev/null").unwrap()))
    }

    /// Creates an empty device-mapper device.
    ///
    /// The device has no active table. Use [`load`](Self::load) and then
    /// [`suspend(dev, false)`](Self::suspend) to activate a mapping.
    /// The device remains until removed, including after process exit; it is not
    /// recreated automatically after reboot.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a name containing NUL or exceeding 127 bytes.
    /// The kernel also rejects invalid or already-used names. Other errors follow
    /// the [connection's error contract](Control#errors).
    pub fn create(&self, name: &str) -> io::Result<DevId> {
        let reply = Command::<0>::new(Header::by_name(name)?).execute(self, Command::DEV_CREATE)?;
        Ok(DevId::from_kernel(reply.header().dev()))
    }

    /// Looks up a device and its whole-device status by name.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for NUL-containing names or names over 127 bytes.
    /// A missing device produces an operating-system error.
    pub fn by_name(&self, name: &str) -> io::Result<(DevId, Status)> {
        self.status_lookup(Header::by_name(name)?)
    }

    /// Looks up a device and its whole-device status by UUID.
    ///
    /// This is the string identifier assigned by [`set_uuid`](Self::set_uuid),
    /// not necessarily an RFC-formatted UUID.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for NUL-containing identifiers or identifiers over
    /// 128 bytes. A missing device produces an operating-system error.
    pub fn by_uuid(&self, uuid: &str) -> io::Result<(DevId, Status)> {
        self.status_lookup(Header::by_uuid(uuid)?)
    }

    #[allow(clippy::large_types_passed_by_value)] // Moves the request into the exchange.
    fn status_lookup(&self, header: Header) -> io::Result<(DevId, Status)> {
        let reply = Command::<0>::new(header).execute(self, Command::DEV_STATUS)?;
        Ok((
            DevId::from_kernel(reply.header().dev()),
            Status::from(reply.header()),
        ))
    }

    /// Reads whole-device state, including suspension and table presence.
    ///
    /// For target-specific runtime information, use [`info`](Self::info).
    /// See [`Status`] for interpreting the result.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the device cannot be queried.
    pub fn status(&self, dev: DevId) -> io::Result<Status> {
        self.status_lookup(Header::by_dev(dev.to_kernel()))
            .map(|(_, status)| status)
    }

    /// Waits for the device's event counter to differ from `event`.
    ///
    /// Pass [`Status::event_nr`] from an earlier [`status`](Self::status) call.
    /// If the counter already differs, this returns immediately. Otherwise it may
    /// block indefinitely; no timeout is provided.
    ///
    /// Only whole-device status is returned, without target rows. The returned
    /// [`Status::flags`] may include the kernel's `DM_BUFFER_FULL_FLAG` (`1 << 8`)
    /// because those rows do not fit. This does not invalidate the returned
    /// status or indicate a device failure. Use [`info`](Self::info) to query
    /// target-specific information separately.
    ///
    /// # Errors
    ///
    /// Returns operating-system errors, including interruption by a signal.
    /// The call does not automatically retry interrupted waits.
    pub fn wait(&self, dev: DevId, event: u32) -> io::Result<Status> {
        let header = Header::by_dev(dev.to_kernel()).with_event_nr(event);
        // Fixed status remains usable even if target rows do not fit.
        let reply = Command::<0>::new(header).execute(self, Command::DEV_WAIT)?;
        Ok(Status::from(reply.header()))
    }

    /// Suspends I/O when `suspend` is true; resumes it when false.
    ///
    /// Suspension waits for outstanding I/O and queues new requests. Resuming
    /// activates the table staged by [`load`](Self::load), if any, and allows
    /// I/O to proceed. These operations can block.
    ///
    /// # Errors
    ///
    /// Returns operating-system errors if suspension or activation fails, including
    /// target-specific activation failures.
    pub fn suspend(&self, dev: DevId, suspend: bool) -> io::Result<()> {
        Command::<0>::new(Header::by_dev(dev.to_kernel()).with_suspend(suspend))
            .execute(self, Command::DEV_SUSPEND)
            .map(|_| ())
    }

    /// Removes a device, immediately or after its last holder closes.
    ///
    /// With `deferred` false, an open device cannot be removed. With it true, the
    /// kernel removes an unused device immediately or schedules removal after its
    /// last holder closes. The request remains effective after process exit.
    ///
    /// Open a holder before requesting deferred removal if the mapping must remain
    /// available for further I/O.
    ///
    /// # Errors
    ///
    /// Immediate removal returns `ResourceBusy` while the device is in use.
    /// Other failures are returned as operating-system errors.
    pub fn remove(&self, dev: DevId, deferred: bool) -> io::Result<()> {
        Command::<0>::new(Header::by_dev(dev.to_kernel()).with_deferred_remove(deferred))
            .execute(self, Command::DEV_REMOVE)
            .map(|_| ())
    }

    /// Discards the inactive table staged by [`load`](Self::load).
    ///
    /// The active mapping is unchanged. Clearing succeeds when the device exists
    /// but has no inactive table.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the device cannot be cleared.
    pub fn clear(&self, dev: DevId) -> io::Result<()> {
        Command::<0>::new(Header::by_dev(dev.to_kernel()))
            .execute(self, Command::TABLE_CLEAR)
            .map(|_| ())
    }

    /// Arms the control descriptor for device-mapper change notifications.
    ///
    /// Obtain the descriptor through [`AsFd`], arm it, then wait for input readiness
    /// with poll/epoll or an async reactor. Readiness is subsystem-wide, not specific
    /// to one device. Re-arm before refreshing the relevant state so changes during
    /// that refresh can trigger another notification.
    ///
    /// Readiness persists until re-armed. Re-arming discards pending readiness;
    /// clones share both the descriptor and its armed state.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error if the kernel rejects the request.
    pub fn arm_poll(&self) -> io::Result<()> {
        Command::<0>::new(Header::any())
            .execute(self, Command::DEV_ARM_POLL)
            .map(|_| ())
    }

    /// Renames a device without changing its identifier.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if either name contains NUL or exceeds 127 bytes.
    /// The kernel rejects unavailable source names and invalid or already-used
    /// replacement names.
    pub fn rename(&self, name: &str, replacement: &str) -> io::Result<DevId> {
        self.rename_inner(name, replacement, false)
    }

    /// Assigns a string identifier to a named device.
    ///
    /// New devices have no UUID. The kernel permits assigning one only once.
    /// The identifier need not have RFC UUID syntax. Use [`by_uuid`](Self::by_uuid)
    /// to look up the device afterward.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for embedded NULs, names over 127 bytes, or UUIDs over
    /// 128 bytes. The kernel rejects missing devices, duplicate UUIDs, and attempts
    /// to replace an assigned UUID.
    pub fn set_uuid(&self, name: &str, uuid: &str) -> io::Result<DevId> {
        self.rename_inner(name, uuid, true)
    }

    fn rename_inner(&self, name: &str, replacement: &str, uuid: bool) -> io::Result<DevId> {
        let limit = if uuid {
            Header::UUID_LEN
        } else {
            Header::NAME_LEN
        };
        if replacement.len() >= limit || replacement.as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid replacement name or uuid",
            ));
        }
        let header = Header::by_name(name)?.with_uuid_flag(uuid);
        let mut data = [0; Header::UUID_LEN];
        data[..replacement.len()].copy_from_slice(replacement.as_bytes());
        let reply = Command::from_parts(header, data).execute(self, Command::DEV_RENAME)?;
        Ok(DevId::from_kernel(reply.header().dev()))
    }

    /// Stages a table for activation, replacing any existing inactive table.
    ///
    /// The active mapping remains unchanged until [`suspend(dev, false)`](Self::suspend).
    /// Set `read_only` to make the new mapping read-only. Use [`clear`](Self::clear)
    /// to discard a staged table. See [`Table`] for construction.
    ///
    /// # Errors
    ///
    /// The kernel validates target availability, parameters, backing devices, and
    /// sector layout. Rejections are returned as operating-system errors.
    /// The supplied table is consumed even on failure.
    pub fn load<const N: usize>(
        &self,
        dev: DevId,
        table: Table<N>,
        read_only: bool,
    ) -> io::Result<()> {
        table
            .into_command(dev, read_only)
            .execute(self, Command::TABLE_LOAD)
            .map(|_| ())
    }

    /// Lists device names and identifiers.
    ///
    /// Names borrow the reply until the next [`Records::next`] call.
    /// See [query buffers](Control#query-buffers) for capacity and ownership.
    ///
    /// # Errors
    ///
    /// The query returns `StorageFull` if `N` is insufficient, or an operating-system
    /// error. Invalid reply data, including non-UTF-8 names, yields `InvalidData`
    /// during iteration and ends traversal.
    pub fn devices<const N: usize>(
        &self,
    ) -> io::Result<impl for<'a> Records<Item<'a> = (&'a str, DevId)> + Debug + use<N>> {
        Devices::<N>::new(Command::new(Header::any()).execute(self, Command::LIST_DEVICES)?)
    }

    /// Queries a kernel target's version, loading its module if available.
    ///
    /// Pass the result to [`Table::add`] or [`Row::parse`]. It identifies the target
    /// implementation, not the device-mapper ioctl protocol. The result can be reused
    /// for that target. [`versions`](Self::versions) lists only registered targets.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for NUL-containing names or names over 127 bytes,
    /// `InvalidData` for a malformed or mismatched reply, and operating-system
    /// errors if the target cannot be queried.
    pub fn version(&self, name: &str) -> io::Result<Version> {
        let reply = Command::<512>::new(Header::by_name(name)?)
            .execute(self, Command::GET_TARGET_VERSION)?;
        let mut versions = Versions::new(reply)?;
        let (reported, version) = versions
            .next()
            .transpose()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing target version"))?;
        if reported != name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected target version",
            ));
        }
        Ok(version)
    }

    /// Lists registered target names and versions.
    ///
    /// A loadable target may be absent until requested with [`version`](Self::version).
    /// See [query buffers](Control#query-buffers) for capacity and ownership.
    ///
    /// # Errors
    ///
    /// The query returns `StorageFull` if `N` is insufficient, or an operating-system
    /// error. Invalid reply data, including non-UTF-8 names, yields `InvalidData`
    /// during iteration and ends traversal.
    pub fn versions<const N: usize>(
        &self,
    ) -> io::Result<impl for<'a> Records<Item<'a> = (&'a str, Version)> + Debug + use<N>> {
        Versions::<N>::new(Command::new(Header::any()).execute(self, Command::LIST_VERSIONS)?)
    }

    /// Lists backing devices referenced by the active table.
    ///
    /// These are direct dependencies, not a recursive traversal.
    /// See [query buffers](Control#query-buffers) for capacity and ownership.
    ///
    /// # Errors
    ///
    /// Returns `StorageFull` if `N` is insufficient, `InvalidData` for a malformed
    /// reply, or an operating-system error.
    pub fn deps<const N: usize>(
        &self,
        dev: DevId,
    ) -> io::Result<impl for<'a> Records<Item<'a> = DevId> + Debug + use<N>> {
        Dependencies::<N>::new(
            Command::new(Header::by_dev(dev.to_kernel())).execute(self, Command::TABLE_DEPS)?,
        )
    }

    /// Reads the active mapping's target parameters.
    ///
    /// Rows are borrowed and parsed on demand with [`Row::parse`], producing
    /// [`Target::Table`](crate::targets::Target::Table). The kernel may normalize or omit
    /// construction parameters; read-back is not necessarily suitable for reloading.
    /// See [query buffers](Control#query-buffers) and the [row example](Row#examples).
    ///
    /// # Errors
    ///
    /// The query returns `StorageFull` if `N` is insufficient, or an operating-system
    /// error. Invalid row framing or non-UTF-8 target names yields `InvalidData`
    /// during iteration and ends traversal. Parameter errors are deferred to
    /// [`Row::parse`].
    pub fn table<const N: usize>(
        &self,
        dev: DevId,
    ) -> io::Result<impl for<'a> Records<Item<'a> = Row<'a, records::Table>> + Debug + use<N>> {
        let header = Header::by_dev(dev.to_kernel()).with_status_table(true);
        Rows::<records::Table, N>::new(Command::new(header).execute(self, Command::TABLE_STATUS)?)
    }

    /// Reads runtime information for each target in the active mapping.
    ///
    /// [`Row::parse`] produces [`Target::Info`](crate::targets::Target::Info), rather than
    /// the construction parameters returned by [`table`](Self::table).
    /// Use [`status`](Self::status) for whole-device state.
    /// See [query buffers](Control#query-buffers) for capacity and ownership.
    ///
    /// # Examples
    ///
    /// The linear target has no runtime fields, represented by `()`.
    ///
    /// ```no_run
    /// use devmap_linux::{Control, DevId, records::Records, targets::Linear};
    ///
    /// fn inspect(control: &Control, id: DevId) -> std::io::Result<()> {
    ///     let mut rows = control.info::<4096>(id)?;
    ///     while let Some(row) = rows.next() {
    ///         let row = row?;
    ///         if row.name() == "linear" {
    ///             row.parse::<Linear>(control.version(row.name())?)?;
    ///         }
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// The query returns `StorageFull` if `N` is insufficient, or an operating-system
    /// error. Invalid row framing or non-UTF-8 target names yields `InvalidData`
    /// during iteration and ends traversal. Parameter errors are deferred to
    /// [`Row::parse`].
    pub fn info<const N: usize>(
        &self,
        dev: DevId,
    ) -> io::Result<impl for<'a> Records<Item<'a> = Row<'a, records::Info>> + Debug + use<N>> {
        Rows::<records::Info, N>::new(
            Command::new(Header::by_dev(dev.to_kernel())).execute(self, Command::TABLE_STATUS)?,
        )
    }
}

/// Borrows the shared control descriptor for readiness registration.
impl AsFd for Control {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_and_uuid_reject_invalid_input_before_ioctl() {
        let control = Control::dummy();
        for (name, replacement) in [
            ("old\0name".to_owned(), "new".to_owned()),
            ("n".repeat(128), "new".to_owned()),
            ("é".repeat(64), "new".to_owned()),
            ("old".to_owned(), "new\0name".to_owned()),
            ("old".to_owned(), "n".repeat(129)),
        ] {
            assert_eq!(
                control.rename(&name, &replacement).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "rename({name:?}, {replacement:?})"
            );
            assert_eq!(
                control.set_uuid(&name, &replacement).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "set_uuid({name:?}, {replacement:?})"
            );
        }
        for replacement in ["n".repeat(128), "é".repeat(64)] {
            assert_eq!(
                control.rename("old", &replacement).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn rename_and_uuid_accept_byte_limits_before_ioctl() {
        let control = Control::dummy();
        let name = "n".repeat(127);
        // /dev/null returns ENOTTY only after local validation accepts the input.
        for replacement in ["n".repeat(127), format!("{}a", "é".repeat(63))] {
            assert_eq!(
                control
                    .rename(&name, &replacement)
                    .unwrap_err()
                    .raw_os_error(),
                Some(25)
            );
        }
        for uuid in ["u".repeat(128), "é".repeat(64)] {
            assert_eq!(
                control.set_uuid(&name, &uuid).unwrap_err().raw_os_error(),
                Some(25)
            );
        }
    }

    #[test]
    fn operation_replies_do_not_borrow_the_connection() {
        fn assert_traits<T: Send + Sync + Debug>(_: &T) {}

        let (devices, versions, deps, table, info) = {
            let connection = Control::dummy();
            let id = DevId::new(252, 1).unwrap();
            (
                connection.devices::<4096>(),
                connection.versions::<4096>(),
                connection.deps::<4096>(id),
                connection.table::<4096>(id),
                connection.info::<4096>(id),
            )
        };
        assert_traits(&devices);
        assert_traits(&versions);
        assert_traits(&deps);
        assert_traits(&table);
        assert_traits(&info);
        // /dev/null rejects ioctls; the result types must still outlive the connection.
        assert_eq!(devices.err().unwrap().raw_os_error(), Some(25));
        assert_eq!(versions.err().unwrap().raw_os_error(), Some(25));
        assert_eq!(deps.err().unwrap().raw_os_error(), Some(25));
        assert_eq!(table.err().unwrap().raw_os_error(), Some(25));
        assert_eq!(info.err().unwrap().raw_os_error(), Some(25));
    }
}
