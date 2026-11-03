// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::Command;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Encoded as `[command, version, body]`.
///
/// Tuple length checks depend on the deserializer. Use [`super::Requests`]
/// at the server boundary to reject extra request elements with Ciborium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<T: Command>(pub T);

impl<T: Command> From<T> for Request<T> {
    fn from(body: T) -> Self {
        Self(body)
    }
}

impl<T: Command + Serialize> Serialize for Request<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (T::NAME, T::VERSION, &self.0).serialize(serializer)
    }
}

impl<'de, T: Command + Deserialize<'de>> Deserialize<'de> for Request<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (name, version, body) = <(String, u16, T)>::deserialize(deserializer)?;
        if name != T::NAME {
            return Err(de::Error::custom(format!("expected command {:?}", T::NAME)));
        }
        if version != T::VERSION {
            return Err(de::Error::custom(format!(
                "unsupported version {version} of command {:?}",
                T::NAME
            )));
        }
        Ok(Self(body))
    }
}
