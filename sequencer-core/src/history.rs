// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! External history identity and version coordinates.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
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
/// The bytes must carry the RFC 4122 UUIDv4 version and variant bits. Text and
/// JSON use the canonical lowercase hyphenated representation.
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

impl FromStr for EraId {
    type Err = EraIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36 || [8, 13, 18, 23].iter().any(|&i| value.as_bytes()[i] != b'-') {
            return Err(EraIdParseError::InvalidText);
        }
        let hex = value.replace('-', "");
        let bytes = alloy_primitives::hex::decode(hex).map_err(|_| EraIdParseError::InvalidText)?;
        Self::try_from(bytes.as_slice())
    }
}

impl Serialize for EraId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EraId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for EraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EraId").field(&self.to_string()).finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EraIdParseError {
    #[error("era id must be a hyphenated UUIDv4")]
    InvalidText,
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
/// Consumers must claim both fields when resuming application history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryVersion {
    pub era_id: EraId,
    pub recovery_generation: RecoveryGeneration,
}

/// The history a consumer holds and the next application input it can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryClaim {
    pub version: HistoryVersion,
    pub next_input: ExecutedInputCount,
}

/// One coherent view of the locally available canonical history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBounds {
    pub version: HistoryVersion,
    pub available_from: ExecutedInputCount,
    pub head: ExecutedInputCount,
}

impl HistoryBounds {
    /// Validate identity before position: equal counts cannot resume a different
    /// history. Every count in the inclusive range is admissible; `head` waits
    /// for the next input.
    pub fn validate(&self, claim: HistoryClaim) -> Result<(), HistoryPolicyError> {
        assert!(
            self.available_from <= self.head,
            "available history base exceeds its head"
        );
        if claim.version.era_id != self.version.era_id {
            return Err(HistoryPolicyError::EraChanged {
                current: self.version,
            });
        }
        if claim.version.recovery_generation != self.version.recovery_generation {
            return Err(HistoryPolicyError::StaleGeneration {
                current: self.version,
            });
        }
        if claim.next_input < self.available_from {
            return Err(HistoryPolicyError::HistoryUnavailable {
                available_from: self.available_from,
            });
        }
        if claim.next_input > self.head {
            return Err(HistoryPolicyError::AheadOfHead { head: self.head });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HistoryPolicyError {
    #[error("history era changed")]
    EraChanged { current: HistoryVersion },
    #[error("history generation changed")]
    StaleGeneration { current: HistoryVersion },
    #[error("requested input precedes locally available history")]
    HistoryUnavailable { available_from: ExecutedInputCount },
    #[error("requested input is ahead of the history head")]
    AheadOfHead { head: ExecutedInputCount },
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
    fn era_and_claim_json_preserve_their_exact_identity() {
        let era: EraId = "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap();
        assert_eq!(
            serde_json::to_string(&era).unwrap(),
            "\"00112233-4455-4677-8899-aabbccddeeff\""
        );
        let claim = HistoryClaim {
            version: HistoryVersion {
                era_id: era,
                recovery_generation: RecoveryGeneration::new(7),
            },
            next_input: ExecutedInputCount::new(u64::MAX),
        };
        assert_eq!(
            serde_json::from_str::<HistoryClaim>(&serde_json::to_string(&claim).unwrap()).unwrap(),
            claim
        );
        assert!(
            "00112233-4455-1677-8899-aabbccddeeff"
                .parse::<EraId>()
                .is_err()
        );
        assert!("00112233445546778899aabbccddeeff".parse::<EraId>().is_err());
    }

    #[test]
    fn history_policy_errors_preserve_literal_wire_codes_and_fields() {
        let current = HistoryVersion {
            era_id: CANONICAL.parse().unwrap(),
            recovery_generation: RecoveryGeneration::new(7),
        };
        for (error, json) in [
            (
                HistoryPolicyError::EraChanged { current },
                serde_json::json!({
                    "code": "ERA_CHANGED",
                    "current": { "era_id": CANONICAL, "recovery_generation": 7 }
                }),
            ),
            (
                HistoryPolicyError::StaleGeneration { current },
                serde_json::json!({
                    "code": "STALE_GENERATION",
                    "current": { "era_id": CANONICAL, "recovery_generation": 7 }
                }),
            ),
            (
                HistoryPolicyError::HistoryUnavailable {
                    available_from: ExecutedInputCount::new(41),
                },
                serde_json::json!({ "code": "HISTORY_UNAVAILABLE", "available_from": 41 }),
            ),
            (
                HistoryPolicyError::AheadOfHead {
                    head: ExecutedInputCount::new(50),
                },
                serde_json::json!({ "code": "AHEAD_OF_HEAD", "head": 50 }),
            ),
        ] {
            assert_eq!(serde_json::to_value(error).unwrap(), json);
            assert_eq!(
                serde_json::from_value::<HistoryPolicyError>(json).unwrap(),
                error
            );
        }
    }

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

    fn history_bounds(base: u64, head: u64) -> HistoryBounds {
        HistoryBounds {
            version: HistoryVersion {
                era_id: EraId::from_bytes(CANONICAL_BYTES).unwrap(),
                recovery_generation: RecoveryGeneration::new(4),
            },
            available_from: ExecutedInputCount::new(base),
            head: ExecutedInputCount::new(head),
        }
    }

    #[test]
    fn history_claim_checks_era_before_generation_and_position() {
        let bounds = history_bounds(41, 45);
        let mut other_era = CANONICAL_BYTES;
        other_era[0] ^= 1;
        let other_era = EraId::from_bytes(other_era).unwrap();
        for (generation, next_input) in [(4, 43), (3, 0), (u64::MAX, u64::MAX)] {
            assert_eq!(
                bounds.validate(HistoryClaim {
                    version: HistoryVersion {
                        era_id: other_era,
                        recovery_generation: RecoveryGeneration::new(generation),
                    },
                    next_input: ExecutedInputCount::new(next_input),
                }),
                Err(HistoryPolicyError::EraChanged {
                    current: bounds.version,
                })
            );
        }
    }

    #[test]
    fn history_claim_requires_equal_generation_before_checking_position() {
        let bounds = history_bounds(41, 45);
        for generation in [0, 3, 5, u64::MAX] {
            for next_input in [0, 43, u64::MAX] {
                assert_eq!(
                    bounds.validate(HistoryClaim {
                        version: HistoryVersion {
                            recovery_generation: RecoveryGeneration::new(generation),
                            ..bounds.version
                        },
                        next_input: ExecutedInputCount::new(next_input),
                    }),
                    Err(HistoryPolicyError::StaleGeneration {
                        current: bounds.version,
                    })
                );
            }
        }
    }

    #[test]
    fn history_claim_rejects_unavailable_and_future_counts() {
        let bounds = history_bounds(41, 45);
        for next_input in [0, 40, 46, u64::MAX] {
            let expected = if next_input < 41 {
                HistoryPolicyError::HistoryUnavailable {
                    available_from: bounds.available_from,
                }
            } else {
                HistoryPolicyError::AheadOfHead { head: bounds.head }
            };
            assert_eq!(
                bounds.validate(HistoryClaim {
                    version: bounds.version,
                    next_input: ExecutedInputCount::new(next_input),
                }),
                Err(expected)
            );
        }
    }

    #[test]
    fn history_claim_accepts_the_full_inclusive_range_without_a_depth_cap() {
        for (base, head, next_input) in [
            (0, 0, 0),
            (41, 45, 41),
            (41, 45, 43),
            (41, 45, 45),
            (0, 100_001, 0),
            (i64::MAX as u64, i64::MAX as u64 + 1, i64::MAX as u64 + 1),
            (u64::MAX, u64::MAX, u64::MAX),
        ] {
            let bounds = history_bounds(base, head);
            assert_eq!(
                bounds.validate(HistoryClaim {
                    version: bounds.version,
                    next_input: ExecutedInputCount::new(next_input),
                }),
                Ok(())
            );
        }
    }

    #[test]
    #[should_panic(expected = "available history base exceeds its head")]
    fn incoherent_history_bounds_fail_loud() {
        let bounds = history_bounds(42, 41);
        let _ = bounds.validate(HistoryClaim {
            version: bounds.version,
            next_input: ExecutedInputCount::new(41),
        });
    }
}
