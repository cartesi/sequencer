use std::ops::Index;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use types::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Voucher(Voucher),
    Notice(Notice),
}

impl Output {
    pub fn abi_decode<T: AsRef<[u8]>>(payload: &T) -> Self {
        let payload = payload.as_ref();
        assert!(
            payload.len() >= 4,
            "encoded output must contain a 4-byte selector, got {} byte(s)",
            payload.len()
        );
        let selector = &payload[..4];
        if selector == Notice::SELECTOR.as_slice() {
            Output::Notice(Notice::abi_decode(payload).expect("failed to decode notice"))
        } else if selector == Voucher::SELECTOR.as_slice() {
            Output::Voucher(Voucher::abi_decode(payload).expect("failed to decode voucher"))
        } else {
            panic!("unknown output selector: {selector:?}");
        }
    }

    pub fn try_notice(&self) -> Option<&Notice> {
        match self {
            Self::Notice(n) => Some(n),
            Self::Voucher(_) => None,
        }
    }

    pub fn expect_notice(&self) -> &Notice {
        self.try_notice()
            .unwrap_or_else(|| panic!("expected voucher {:?} to be a notice", self))
    }

    pub fn try_voucher(&self) -> Option<&Voucher> {
        match self {
            Self::Notice(_) => None,
            Self::Voucher(v) => Some(v),
        }
    }

    pub fn expect_voucher(&self) -> &Voucher {
        self.try_voucher()
            .unwrap_or_else(|| panic!("expected notice {:?} to be a voucher", self))
    }
}

#[derive(Clone, Debug, Default)]
pub struct OutputsForInput {
    list: Vec<Output>,
}

impl Index<usize> for OutputsForInput {
    type Output = Output;

    fn index(&self, index: usize) -> &Self::Output {
        &self.list[index]
    }
}

impl OutputsForInput {
    pub fn push(&mut self, output: Output) {
        self.list.push(output);
    }

    pub fn push_encoded<T: AsRef<[u8]>>(&mut self, encoded_output: &T) {
        self.push(Output::abi_decode(encoded_output));
    }

    pub fn list(&self) -> &Vec<Output> {
        &self.list
    }

    pub fn notices(&self) -> Vec<&Notice> {
        self.list.iter().filter_map(|x| x.try_notice()).collect()
    }

    pub fn vouchers(&self) -> Vec<&Voucher> {
        self.list.iter().filter_map(|x| x.try_voucher()).collect()
    }
}

#[derive(Clone, Debug)]
pub struct InputBuilder {
    pub sender: Address,
    pub prev_randao: U256,
    pub block_number: U256,
    pub block_timestamp: U256,
    pub payload: Vec<u8>,
}

impl InputBuilder {
    pub fn from_address(sender: Address) -> Self {
        Self {
            sender,
            prev_randao: U256::ZERO,
            block_number: U256::ZERO,
            block_timestamp: U256::ZERO,
            payload: Vec::new(),
        }
    }

    pub fn at_block(mut self, block: usize) -> Self {
        self.block_number = block.try_into().unwrap();
        self
    }

    pub fn with_payload<T: AsRef<[u8]>>(mut self, payload: &T) -> Self {
        self.payload = payload.as_ref().into();
        self
    }

    pub fn with_block_timestamp(mut self, block_timestamp: usize) -> Self {
        self.block_timestamp = block_timestamp.try_into().unwrap();
        self
    }

    pub fn encode(self, chain_id: usize, input_index: U256, dapp: Address) -> Vec<u8> {
        let x = Input::new((
            U256::from(chain_id),
            dapp,
            self.sender,
            self.block_number,
            self.block_timestamp,
            self.prev_randao,
            input_index,
            self.payload.into(),
        ));

        x.abi_encode()
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    #[should_panic(expected = "encoded output must contain a 4-byte selector")]
    fn output_decode_panics_cleanly_on_short_payload() {
        let _ = Output::abi_decode(&[0_u8, 1, 2]);
    }
}
