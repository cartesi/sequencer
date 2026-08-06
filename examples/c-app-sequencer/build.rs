// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Tells the generic binary whether an engine archive was supplied.
//!
//! A cargo feature would be the usual way, but features are additive and `--all-features` would
//! turn it on in builds with no archive, which is exactly the combination that cannot link.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(external_engine)");
    println!("cargo::rerun-if-env-changed=APPLICATION_ENGINE_LIB");
    if std::env::var_os("APPLICATION_ENGINE_LIB").is_some() {
        println!("cargo::rustc-cfg=external_engine");
    }
}
