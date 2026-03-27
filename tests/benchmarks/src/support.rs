// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::error::Error;

pub fn io_err(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}

/// Extract the trailing integer from a filename stem.
///
/// e.g. `"ack-latency-1774724681.json"` → `Some(1774724681)`
pub fn trailing_number(file_name: &str) -> Option<u64> {
    let stem = file_name
        .rsplit_once('.')
        .map(|(l, _)| l)
        .unwrap_or(file_name);
    let reversed_digits: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if reversed_digits.is_empty() {
        return None;
    }
    let digits: String = reversed_digits.chars().rev().collect();
    digits.parse().ok()
}
