// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::error::Error;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_PROGRESS_EVERY: u64 = 500;

pub fn now() -> Instant {
    Instant::now()
}

pub fn default_seed_offset() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub fn io_err(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}

pub fn err(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}
