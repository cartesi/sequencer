// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inbound side: public HTTP (`POST /tx`, `GET /fee`) and the inclusion lane
//! that consumes the submit queue. The lane is the only writer of open
//! batch/frame state in storage.

pub mod api;
pub mod inclusion_lane;
