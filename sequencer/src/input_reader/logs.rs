// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::sol_types::SolCall;
use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

pub(crate) fn decode_evm_advance_input(input: &[u8]) -> Result<EvmAdvanceCall, String> {
    EvmAdvanceCall::abi_decode(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use alloy_sol_types::SolCall;
    use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

    use super::decode_evm_advance_input;

    #[test]
    fn decode_evm_advance_input_round_trips() {
        let encoded = EvmAdvanceCall {
            chainId: U256::from(31337_u64),
            appContract: address!("0x1111111111111111111111111111111111111111"),
            msgSender: address!("0x2222222222222222222222222222222222222222"),
            blockNumber: U256::from(99_u64),
            blockTimestamp: U256::from(1234_u64),
            prevRandao: U256::from(7_u64),
            index: U256::from(3_u64),
            payload: vec![0xaa, 0xbb].into(),
        }
        .abi_encode();

        let decoded = decode_evm_advance_input(encoded.as_slice()).expect("decode evm advance");
        assert_eq!(
            decoded.msgSender,
            address!("0x2222222222222222222222222222222222222222")
        );
        assert_eq!(decoded.blockNumber, U256::from(99_u64));
        assert_eq!(decoded.payload.as_ref(), &[0xaa, 0xbb]);
    }
}
