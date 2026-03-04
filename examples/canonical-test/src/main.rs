// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use canonical_app::SEQUENCER_ADDRESS;
use std::path::PathBuf;
use types::alloy_primitives::Address;

testsi::testsi_main!();

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_reports_invalid_batch_from_guest() -> testsi::TestResult {
    let machine_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../canonical-app/out/canonical-machine-image");
    let mut machine = testsi::MachineBuilder::load_from(machine_path)
        .at_chain(31337)
        .deployed_at(Address::ZERO)
        .no_console_putchar(false)
        .try_build()?;

    let input = testsi::InputBuilder::from_address(SEQUENCER_ADDRESS)
        .at_block(10)
        .with_payload(&[0xff, 0xee, 0xdd]);

    let (outputs, reports) = machine.advance_state(input)?;

    assert!(
        outputs.list().is_empty(),
        "invalid batch smoke test should not emit notices/vouchers, got {:?}",
        outputs.list()
    );
    assert!(
        reports
            .iter()
            .any(|report| report.as_slice() == b"scheduler dropped invalid batch"),
        "expected invalid batch report, got {reports:?}"
    );

    Ok(())
}
