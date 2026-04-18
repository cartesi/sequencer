// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared fixtures for `sequencer/tests/*.rs` integration tests.
//!
//! Integration tests compile as separate crates and cannot reach the
//! `#[cfg(test)]` helpers inside `sequencer/src/`. This module keeps the same
//! `TestDb` shape so callers work identically on both sides.

use tempfile::TempDir;

pub struct TestDb {
    pub _dir: TempDir,
    pub path: String,
}

pub fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}
