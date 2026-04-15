// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Outbound side: WS subscribe (today), future read-only endpoints, and the
//! L2-tx feed that backs them. Operated for internal indexers; the future api
//! split puts these on a separate port from ingress.

pub mod api;
pub mod l2_tx_feed;
