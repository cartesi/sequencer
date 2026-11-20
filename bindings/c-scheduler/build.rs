// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Generates the C records and declarations from `include/scheduler.h`, so the export and the
//! header cannot drift: a header change either reaches the export or fails this build.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let header = manifest_dir.join("include").join("scheduler.h");
    // The transaction records are the engine's own, from the engine header beside this crate.
    let engine_include = manifest_dir.join("../c-app-engine/include");
    println!("cargo::rerun-if-changed={}", header.display());
    println!(
        "cargo::rerun-if-changed={}",
        engine_include.join("application-engine.h").display()
    );

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("bindings.rs");
    bindgen::Builder::default()
        .header(
            header
                .to_str()
                .expect("scheduler.h path is not valid UTF-8"),
        )
        // The C arm of the header; its C++ arm only spells noexcept.
        .clang_args(["-x", "c", "-std=c11"])
        .clang_arg(format!("-I{}", engine_include.display()))
        // The scheduler's surface, and the engine records it reaches, never stdint.h's.
        .allowlist_item("^(scheduler_|Scheduler|SCHEDULER_).*")
        // Integer constants, never Rust enums: a value the header adds later must be refusable.
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        // Records are written whole, so they start from zeroes.
        .derive_default(true)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .generate()
        .expect("failed to generate bindings from scheduler.h")
        .write_to_file(&out_path)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", out_path.display()));
}
