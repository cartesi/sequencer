use core::fmt;
use types::alloy_primitives::{Address, U256};

pub mod cmt;

#[derive(Debug)]
pub struct InputMetadata {
    pub chain_id: U256,
    pub app_contract: Address,
    pub msg_sender: Address,
    pub block_number: U256,
    pub block_timestamp: U256,
    pub prev_randao: U256,
    pub index: U256,
}

#[derive(Debug)]
pub enum RollupRequest {
    Advance {
        metadata: InputMetadata,
        payload: Vec<u8>,
    },
    Inspect {
        payload: Vec<u8>,
    },
}

pub type RollupResult<T> = Result<T, RollupError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollupError {
    CmtCallFailed {
        operation: &'static str,
        code: i32,
    },
    UnexpectedRequestType {
        request_type: u32,
    },
    LengthOverflow {
        field: &'static str,
        len: usize,
        max: usize,
    },
    InvalidPayloadPointer {
        field: &'static str,
        len: usize,
    },
}

impl fmt::Display for RollupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CmtCallFailed { operation, code } => {
                write!(f, "{operation} failed with rc={code}")
            }
            Self::UnexpectedRequestType { request_type } => {
                write!(f, "unknown request type from host: {request_type}")
            }
            Self::LengthOverflow { field, len, max } => {
                write!(f, "{field} length overflow: {len} > {max}")
            }
            Self::InvalidPayloadPointer { field, len } => {
                write!(
                    f,
                    "{field} returned null pointer with non-zero length {len}"
                )
            }
        }
    }
}

impl std::error::Error for RollupError {}

pub trait Rollup {
    fn next_input(&mut self) -> RollupResult<RollupRequest>;
    fn revert(&mut self) -> !;
    fn gio(&mut self, domain: u16, id: &[u8]) -> RollupResult<(u16, Vec<u8>)>;
    fn emit_voucher(&mut self, voucher: &types::Voucher) -> RollupResult<()>;
    fn emit_notice(&mut self, notice: &types::Notice) -> RollupResult<()>;
    fn emit_report(&mut self, report: &[u8]) -> RollupResult<()>;
}
