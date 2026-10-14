// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime authority capabilities (authority-boundary ADR, mechanism 1): the
//! exclusive data-directory lock ([`process_lock`]) and `RuntimeScope` with
//! cooperative shutdown and terminal abort ([`shutdown`]).

pub(crate) mod process_lock;
pub mod shutdown;
