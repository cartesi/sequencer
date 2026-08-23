// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! External history identity and version coordinates.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Boundary before the next canonical application input executes.
///
/// If the application is at `X`, history entry `X` is the next input and a
/// successful execution moves the boundary to `X + 1`. Arithmetic is kept
/// behind checked methods so this coordinate cannot be confused with a
/// physical SQLite cursor or advanced with wrapping/saturating math.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ExecutedInputCount(u64);

impl ExecutedInputCount {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        self.checked_add(1)
    }

    pub const fn checked_add(self, delta: u64) -> Option<Self> {
        match self.0.checked_add(delta) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// One durable setup/rebuild era.
///
/// The bytes must carry the RFC 4122 UUIDv4 version and variant bits. Display
/// uses the canonical lowercase hyphenated representation. The wire (text /
/// JSON) codec deliberately does not exist yet: Track 3 owns the wire
/// projection and adds it beside its consumer when that lands.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EraId([u8; 16]);

impl EraId {
    pub const BYTE_LEN: usize = 16;

    pub fn from_bytes(bytes: [u8; Self::BYTE_LEN]) -> Result<Self, EraIdParseError> {
        if bytes[6] >> 4 != 4 {
            return Err(EraIdParseError::NotVersion4);
        }
        if bytes[8] >> 6 != 2 {
            return Err(EraIdParseError::InvalidVariant);
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; Self::BYTE_LEN] {
        &self.0
    }
}

impl TryFrom<&[u8]> for EraId {
    type Error = EraIdParseError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let bytes: [u8; Self::BYTE_LEN] =
            value
                .try_into()
                .map_err(|_| EraIdParseError::InvalidByteLength {
                    actual: value.len(),
                })?;
        Self::from_bytes(bytes)
    }
}

impl fmt::Display for EraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for EraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EraId").field(&self.to_string()).finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EraIdParseError {
    #[error("era id blob has length {actual}, expected 16")]
    InvalidByteLength { actual: usize },
    #[error("era id is not UUID version 4")]
    NotVersion4,
    #[error("era id has a non-RFC-4122 UUID variant")]
    InvalidVariant,
}

/// Monotonic soft-history revision within one [`EraId`].
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RecoveryGeneration(u64);

impl RecoveryGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Equality/discontinuity token for locally available application history.
/// Like [`EraId`], its wire form is Track 3's to define beside its consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HistoryVersion {
    pub era_id: EraId,
    pub recovery_generation: RecoveryGeneration,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL: &str = "550e8400-e29b-41d4-a716-446655440000";
    const CANONICAL_BYTES: [u8; 16] = [
        0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00,
        0x00,
    ];

    #[test]
    fn era_id_displays_canonical_lowercase_hyphenated_form() {
        let era = EraId::from_bytes(CANONICAL_BYTES).expect("canonical UUIDv4");
        assert_eq!(era.to_string(), CANONICAL);
    }

    #[test]
    fn era_id_rejects_non_v4_bytes() {
        let mut not_v4 = CANONICAL_BYTES;
        not_v4[6] = 0x31;
        assert_eq!(EraId::from_bytes(not_v4), Err(EraIdParseError::NotVersion4));
        let mut bad_variant = CANONICAL_BYTES;
        bad_variant[8] = 0x07;
        assert_eq!(
            EraId::from_bytes(bad_variant),
            Err(EraIdParseError::InvalidVariant)
        );
        assert_eq!(
            EraId::try_from(&[0_u8; 15][..]),
            Err(EraIdParseError::InvalidByteLength { actual: 15 })
        );
    }

    #[test]
    fn executed_input_count_advances_checked() {
        assert_eq!(
            ExecutedInputCount::ZERO.checked_next(),
            Some(ExecutedInputCount::new(1))
        );
        assert_eq!(ExecutedInputCount::new(u64::MAX).checked_next(), None);
        assert_eq!(
            ExecutedInputCount::new(7).checked_add(5),
            Some(ExecutedInputCount::new(12))
        );
    }
}
