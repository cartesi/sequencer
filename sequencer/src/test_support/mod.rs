// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared `#[cfg(test)]` fixtures for the sequencer crate.

mod stub_app;

pub(crate) use stub_app::{ReplayEvent, StubApp, create_structured_dump};
