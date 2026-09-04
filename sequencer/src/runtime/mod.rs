// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The runtime authority capabilities — the ADR's "structured process
//! ownership" mechanism, and nothing else:
//!
//! - [`process_lock`] — the exclusive kernel-enforced data-directory lock
//!   every command and nested blocking task retains until it truly stops.
//! - [`shutdown`] — `RuntimeScope` (lock + terminal-abort watchdog +
//!   containment authority) and the slim cooperative
//!   `ShutdownSignal`.
//!
//! Everything here is consumed crate-wide (workers, egress, l1, recovery);
//! the command brackets live in [`crate::commands`], which also owns the
//! command-scoped config and error taxonomy.

pub(crate) mod process_lock;
pub mod shutdown;
