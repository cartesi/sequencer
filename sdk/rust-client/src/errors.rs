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
    pub fn breakdown_key(&self) -> &'static str {
        match self {
            Self::TimeoutConnect => "timeout_connect",
            Self::TimeoutWrite => "timeout_write",
            Self::TimeoutFlush => "timeout_flush",
            Self::TimeoutRead => "timeout_read",
            Self::IoConnect(_) => "io_connect",
            Self::IoWrite(_) => "io_write",
            Self::IoFlush(_) => "io_flush",
            Self::IoRead(_) => "io_read",
            Self::Parse(_) => "parse_error",
        }
    }
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
