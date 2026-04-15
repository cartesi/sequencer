// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inbound side: HTTP submit endpoint and the inclusion lane that consumes its
//! queue. The submit API is the public-facing port; the lane is the only writer
//! of open batch/frame state in storage.

pub mod api;
pub mod inclusion_lane;
