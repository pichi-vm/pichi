// SPDX-License-Identifier: Apache-2.0

use std::{fmt, ops::Deref};

const MAX_SIZE: usize = 256;

/// Salt bytes recorded in a dm-verity header.
///
/// Holds up to 256 bytes; the default is an empty salt. Borrowing as a byte
/// slice exposes only the supplied bytes, not the unused storage.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Salt([u8; MAX_SIZE], usize);

impl Salt {
    /// Constructs a salt when the bytes fit in the on-disk header.
    ///
    /// Returns `None` for more than 256 bytes. An empty slice is accepted.
    pub fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_SIZE {
            return None;
        }

        let mut storage = [0; MAX_SIZE];
        storage[..bytes.len()].copy_from_slice(bytes);
        Some(Self(storage, bytes.len()))
    }
}

impl Default for Salt {
    fn default() -> Self {
        Self([0; MAX_SIZE], 0)
    }
}

impl fmt::Debug for Salt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Salt").field(&self.as_ref()).finish()
    }
}

impl Deref for Salt {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0[..self.1]
    }
}

impl AsRef<[u8]> for Salt {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::Salt;

    #[test]
    fn debug_shows_only_salt_bytes() {
        assert_eq!(format!("{:?}", Salt::default()), "Salt([])");
        assert_eq!(
            format!("{:?}", Salt::new(&[1, 0, 3]).unwrap()),
            "Salt([1, 0, 3])"
        );
    }
}
