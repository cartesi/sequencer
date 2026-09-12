// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

#[test]
fn help_does_not_create_a_genesis_dump() {
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_c-wallet-genesis"))
            .arg(flag)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("usage:"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
