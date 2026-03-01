// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Reads safe (direct) inputs from a reference source (e.g. InputBox contract) and appends them
//! to sequencer storage. Minimal design: no epochs or consensus; flat contiguous indices only.

mod reader;

pub use reader::{InputReader, InputReaderConfig, InputReaderError};
