// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Outbound side: the internal listener's subscription, history, snapshot and
//! health routes, and the L2-tx feed that backs them. Operated for internal
//! indexers and the watchdog.

pub mod api;
pub mod l2_tx_feed;
