// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Encoded as `[true, value]` or `[false, message]`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged, bound(deserialize = "T: Deserialize<'de>"))]
pub enum Response<T> {
    #[serde(deserialize_with = "Self::deserialize_ok")]
    Ok(T),

    #[serde(deserialize_with = "Self::deserialize_err")]
    Err(String),
}

impl<T> From<Result<T, String>> for Response<T> {
    fn from(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Err(error),
        }
    }
}

impl<T> From<Response<T>> for Result<T, String> {
    fn from(response: Response<T>) -> Self {
        match response {
            Response::Ok(value) => Ok(value),
            Response::Err(error) => Err(error),
        }
    }
}

impl<T: Serialize> Serialize for Response<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Ok(value) => (true, value).serialize(serializer),
            Self::Err(error) => (false, error).serialize(serializer),
        }
    }
}

impl<T> Response<T> {
    fn deserialize_ok<'de, D: Deserializer<'de>>(deserializer: D) -> Result<T, D::Error>
    where
        T: Deserialize<'de>,
    {
        let (ok, value) = <(bool, T)>::deserialize(deserializer)?;
        if !ok {
            return Err(de::Error::custom("expected success status"));
        }
        Ok(value)
    }

    fn deserialize_err<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let (ok, error) = <(bool, String)>::deserialize(deserializer)?;
        if ok {
            return Err(de::Error::custom("expected error status"));
        }
        Ok(error)
    }
}
