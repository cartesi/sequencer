// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientBuildError {
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
}

#[derive(Debug, Error)]
pub enum SubmitTxError {
    #[error("tcp connect timeout")]
    TimeoutConnect,
    #[error("tcp write timeout")]
    TimeoutWrite,
    #[error("tcp flush timeout")]
    TimeoutFlush,
    #[error("tcp read timeout")]
    TimeoutRead,
    #[error("tcp connect failed: {0}")]
    IoConnect(String),
    #[error("tcp write failed: {0}")]
    IoWrite(String),
    #[error("tcp flush failed: {0}")]
    IoFlush(String),
    #[error("tcp read failed: {0}")]
    IoRead(String),
    #[error("parse failed: {0}")]
    Parse(String),
}

impl SubmitTxError {
    pub fn breakdown_key(&self) -> String {
        match self {
            Self::TimeoutConnect => "timeout_connect".to_string(),
            Self::TimeoutWrite => "timeout_write".to_string(),
            Self::TimeoutFlush => "timeout_flush".to_string(),
            Self::TimeoutRead => "timeout_read".to_string(),
            Self::IoConnect(msg) => format!("io_connect({})", classify_io_error(msg)),
            Self::IoWrite(msg) => format!("io_write({})", classify_io_error(msg)),
            Self::IoFlush(msg) => format!("io_flush({})", classify_io_error(msg)),
            Self::IoRead(msg) => format!("io_read({})", classify_io_error(msg)),
            Self::Parse(_) => "parse_error".to_string(),
        }
    }
}

/// Extract a short, groupable cause from a reqwest/hyper error message.
fn classify_io_error(msg: &str) -> &str {
    let lower = msg.as_bytes();
    // Check common patterns in reqwest/hyper error strings.
    if contains_ignore_case(lower, b"connection refused") {
        "connection_refused"
    } else if contains_ignore_case(lower, b"connection reset") {
        "connection_reset"
    } else if contains_ignore_case(lower, b"broken pipe") {
        "broken_pipe"
    } else if contains_ignore_case(lower, b"too many open files") {
        "too_many_open_files"
    } else if contains_ignore_case(lower, b"dns") || contains_ignore_case(lower, b"resolve") {
        "dns_error"
    } else if contains_ignore_case(lower, b"timed out") || contains_ignore_case(lower, b"timeout")
    {
        "timed_out"
    } else if contains_ignore_case(lower, b"closed") {
        "connection_closed"
    } else if contains_ignore_case(lower, b"eof") {
        "unexpected_eof"
    } else {
        "other"
    }
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

#[derive(Debug, Error)]
pub enum SubmitRejected {
    #[error("tx submit failed: {0}")]
    Transport(#[from] SubmitTxError),
    #[error("/tx rejected with status {status}: {body}")]
    Http { status: u16, body: String },
    #[error("invalid /tx success body: {0}")]
    Decode(String),
}

#[derive(Debug, Error)]
pub enum SubscribeError {
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("ws connect failed: {0}")]
    Connect(String),
}
