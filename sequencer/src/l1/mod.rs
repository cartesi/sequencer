// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! L1 client surface: reads InputBox events into storage (`reader`), submits
//! batches back out (`submitter`), and shares L1 utilities (`provider`,
//! `partition` — range partition retry + count-guided scan).

pub mod partition;
pub mod provider;
pub mod reader;
pub mod submitter;
pub mod watermark;
