// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::os::fd::AsFd;

use iocuddle::{Group, Ioctl, Size, WriteRead};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::Header;

/// An entire ioctl exchange, including N bytes of payload capacity.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct Command<const N: usize> {
    header: Header,
    data: [u8; N],
}

impl<const N: usize> Command<N> {
    // The kernel's data_size field covers the entire exchange.
    #[allow(clippy::cast_possible_truncation)] // Bounded by the const assertion.
    const CAPACITY: u32 = {
        assert!(N <= u32::MAX as usize - Header::SIZE);
        (Header::SIZE + N) as u32
    };

    // SAFETY: _IOWR(0xfd, 2, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const LIST_DEVICES: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(2) };

    // SAFETY: _IOWR(0xfd, 3, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_CREATE: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(3) };

    // SAFETY: _IOWR(0xfd, 4, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_REMOVE: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(4) };

    // SAFETY: _IOWR(0xfd, 5, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_RENAME: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(5) };

    // SAFETY: _IOWR(0xfd, 6, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_SUSPEND: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(6) };

    // SAFETY: _IOWR(0xfd, 7, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_STATUS: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(7) };

    // SAFETY: _IOWR(0xfd, 8, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_WAIT: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(8) };

    // SAFETY: _IOWR(0xfd, 9, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const TABLE_LOAD: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(9) };

    // SAFETY: _IOWR(0xfd, 10, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const TABLE_CLEAR: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(10) };

    // SAFETY: _IOWR(0xfd, 11, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const TABLE_DEPS: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(11) };

    // SAFETY: _IOWR(0xfd, 12, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const TABLE_STATUS: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(12) };

    // SAFETY: _IOWR(0xfd, 13, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const LIST_VERSIONS: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(13) };

    // SAFETY: _IOWR(0xfd, 16, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const DEV_ARM_POLL: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(16) };

    // SAFETY: _IOWR(0xfd, 17, struct dm_ioctl). execute bounds data_size
    // to the complete initialized Command before passing it to the kernel.
    #[expect(unsafe_code, reason = "declares the documented device-mapper ABI")]
    pub(crate) const GET_TARGET_VERSION: Ioctl<WriteRead, &'static Self> =
        unsafe { Group::new(0xfd).write_read(17) };

    #[allow(clippy::large_types_passed_by_value)] // Moves the header into its command.
    pub(crate) fn new(header: Header) -> Self {
        Self::from_parts(header, [0; N])
    }

    #[allow(clippy::large_types_passed_by_value)] // Moves the header and payload into the exchange.
    pub(crate) fn from_parts(header: Header, data: [u8; N]) -> Self {
        Self { header, data }
    }

    /// Submit the request and return the validated kernel response.
    pub(crate) fn execute(
        mut self,
        fd: &impl AsFd,
        request: Ioctl<WriteRead, &Self>,
    ) -> io::Result<Self> {
        self.header.set_data_size(Self::CAPACITY);
        request.ioctl(fd, &mut self)?;
        self.validate()?;
        Ok(self)
    }

    pub(crate) fn header(&self) -> &Header {
        &self.header
    }

    fn validate(&self) -> io::Result<()> {
        if self.header.major_version() != Header::VERSION_MAJOR {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported device-mapper ioctl version",
            ));
        }
        let size = self.header.data_size() as usize;
        if size < Header::MIN_SIZE || size > Header::SIZE + N {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "device-mapper response is outside command capacity",
            ));
        }
        Ok(())
    }

    pub(crate) fn payload(&self) -> io::Result<&[u8]> {
        self.validate()?;
        if self.header.flags() & Header::BUFFER_FULL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device-mapper response exceeds command capacity",
            ));
        }
        let start = self.header.data_start() as usize;
        let end = self.header.data_size() as usize;
        // Header-only replies end at offsetof(struct dm_ioctl, data).
        if end == Header::MIN_SIZE {
            return Ok(&[]);
        }
        if start < Header::SIZE || start > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "device-mapper response is outside command capacity",
            ));
        }
        Ok(&self.data[start - Header::SIZE..end - Header::SIZE])
    }
}

impl<const N: usize> Size for Command<N> {
    // Device-mapper request numbers encode sizeof(struct dm_ioctl), not the
    // payload capacity. Header's layout assertions fix this at 312 bytes;
    // execute supplies the full exchange size separately through data_size.
    const SIZE: usize = Header::SIZE;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_preserves_ioctl_errors() {
        let fd = std::fs::File::open("/dev/null").unwrap();
        let mut command = Command::<17>::new(Header::any());
        command.header.set_data_size(u32::MAX);
        let error = command.execute(&fd, Command::LIST_DEVICES).err().unwrap();
        assert_eq!(error.raw_os_error(), Some(25)); // ENOTTY from /dev/null.
    }

    #[test]
    fn inline_layout_has_no_padding_for_any_capacity() {
        assert_eq!(std::mem::size_of::<Command<0>>(), Header::SIZE);
        assert_eq!(std::mem::size_of::<Command<1>>(), Header::SIZE + 1);
        assert_eq!(std::mem::size_of::<Command<129>>(), Header::SIZE + 129);
        assert_eq!(std::mem::offset_of!(Command<129>, data), Header::SIZE);
        assert_eq!(std::mem::align_of::<Command<129>>(), 1);
        let command = Command::<129>::new(Header::any());
        assert!(command.data.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn encoded_size_is_independent_of_payload_capacity() {
        assert_eq!(<Command<0> as Size>::SIZE, Header::SIZE);
        assert_eq!(<Command<1> as Size>::SIZE, Header::SIZE);
        assert_eq!(<Command<65536> as Size>::SIZE, Header::SIZE);
        // The full argument exceeds even the widest ioctl size field.
        let _ = Command::<65536>::TABLE_LOAD;
        assert_eq!(Command::<65536>::CAPACITY, 312 + 65536);
    }

    #[test]
    fn header_only_kernel_reply_has_no_payload() {
        let mut command = Command::<0>::new(Header::any());
        command.header.set_data_size(305);
        assert_eq!(command.payload().unwrap(), []);
    }

    #[test]
    fn payload_is_bounded_by_reported_size_and_capacity() {
        let mut command = Command::<17>::new(Header::any());
        command.header.set_data_size(329);
        assert_eq!(command.payload().unwrap(), [0; 17]);
        command.header.set_data_size(320);
        assert_eq!(command.payload().unwrap(), [0; 8]);
        for size in [0, 304, 306, 311, 330, u32::MAX] {
            command.header.set_data_size(size);
            assert_eq!(
                command.payload().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        command.header.set_data_size(329);
        for start in [0u32, 305, 330, u32::MAX] {
            command.as_mut_bytes()[16..20].copy_from_slice(&start.to_ne_bytes());
            assert_eq!(
                command.payload().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn full_buffer_and_version_mismatch_are_errors() {
        let mut command = Command::<1>::new(Header::any());
        command.as_mut_bytes()[28..32].copy_from_slice(&Header::BUFFER_FULL.to_ne_bytes());
        // A full response need not have usable payload bounds.
        command.header.set_data_size(305);
        assert!(command.validate().is_ok()); // Fixed status remains usable.
        assert_eq!(
            command.payload().unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        command.as_mut_bytes()[28..32].fill(0);
        command.as_mut_bytes()[..4].copy_from_slice(&(Header::VERSION_MAJOR + 1).to_ne_bytes());
        assert_eq!(
            command.payload().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
