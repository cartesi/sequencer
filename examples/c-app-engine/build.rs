// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Links the application's engine archive and generates the FFI declarations from its header.
//!
//! The three environment variables are the whole application-specific binding, documented in
//! `docs/protocol/c-application-binding.md`. With none of them set this crate links no archive,
//! and the binary that uses it supplies the engine instead.

use std::env;
use std::path::{Path, PathBuf};

/// The in-workspace wallet engine's own bound, used when a build declares none.
///
/// Only reachable when `c-wallet-engine` is the engine, which asserts this same value against
/// `WalletApp::MAX_METHOD_PAYLOAD_BYTES`, so a number that drifts fails that crate's compile
/// rather than reaching a host. An application outside this workspace always declares its own.
const REFERENCE_ENGINE_METHOD_PAYLOAD_LIMIT: u32 = 1 + 32 + 20;

/// A ceiling on what an application may declare, since the bound gates ingress.
const MAX_METHOD_PAYLOAD_LIMIT: u32 = 1 << 20;

/// Generate `sys`'s contents from the engine header, the one authoritative declaration of what
/// the archive exports, so a change on the engine side is either picked up here or fails this
/// build.
///
/// The payload bound is defined for the parse rather than read out of the header, because the
/// header deliberately refuses to carry a default for it.
fn generate_bindings(header: &Path, method_payload_limit: u32) {
    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("bindings.rs");
    let bindings = bindgen::Builder::default()
        .header(
            header
                .to_str()
                .expect("APPLICATION_ENGINE_HEADER is not valid UTF-8"),
        )
        // Parse the C arm of the header. Its C++ arm only spells noexcept, which has no bearing
        // on the ABI and no Rust spelling.
        .clang_args(["-x", "c", "-std=c11"])
        .clang_arg(format!(
            "-DAPPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT={method_payload_limit}"
        ))
        // Only the seam's own surface, never what stdint.h drags in behind it
        .allowlist_item("^(application_engine_|ApplicationEngine|APPLICATION_ENGINE_).*")
        // Plain integer constants, never Rust enums. The contract requires refusing a value the
        // engine added later, which holding it in a Rust enum would make undefined behavior.
        .default_enum_style(bindgen::EnumVariation::Consts)
        // The C constants already carry the APPLICATION_ENGINE_ prefix, so repeating the enum's
        // name in front of them would spell them differently here than in the header
        .prepend_enum_name(false)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .generate()
        .expect("failed to generate bindings from APPLICATION_ENGINE_HEADER");
    bindings
        .write_to_file(&out_path)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", out_path.display()));
}

/// Link the application's own archive, the path a deployment outside this workspace takes.
///
/// The link name is the archive's own name, the linker has no other way to spell it.
fn link_application_archive(engine_lib: &Path) {
    // Fail loudly at build time instead of at link time with a confusing message
    assert!(
        engine_lib.is_file(),
        "APPLICATION_ENGINE_LIB points at {}, which is not a file, build the engine archive first",
        engine_lib.display()
    );

    let file_name = engine_lib
        .file_name()
        .and_then(|name| name.to_str())
        .expect("APPLICATION_ENGINE_LIB is not valid UTF-8");
    let link_name = file_name
        .strip_prefix("lib")
        .and_then(|name| name.strip_suffix(".a"))
        .unwrap_or_else(|| {
            panic!("APPLICATION_ENGINE_LIB names {file_name}, expected a static library named lib<name>.a")
        });
    // A bare file name has an empty parent, search the working directory then
    let engine_lib_dir = engine_lib
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    println!("cargo::rerun-if-changed={}", engine_lib.display());
    println!(
        "cargo::rustc-link-search=native={}",
        engine_lib_dir.display()
    );
    println!("cargo::rustc-link-lib=static={link_name}");

    // Engines are commonly implemented in C or C++, and one that needs no C++ runtime links this
    // harmlessly
    let cxx_runtime = match env::var("CARGO_CFG_TARGET_OS").expect("target os").as_str() {
        "macos" => "c++",
        _ => "stdc++",
    };
    println!("cargo::rustc-link-lib={cxx_runtime}");
}

/// What an application supplying its own archive has to declare alongside it.
///
/// Both are demanded rather than defaulted. The archive and the header are separate artifacts and
/// only that pairing is meaningful, and a bound guessed here would be exactly the silently wrong
/// number the header's own `#error` exists to prevent.
fn external_engine(engine_lib: &str) -> (PathBuf, u32) {
    link_application_archive(Path::new(engine_lib));

    let header = PathBuf::from(env::var("APPLICATION_ENGINE_HEADER").expect(
        "APPLICATION_ENGINE_HEADER is unset, point it at the header the archive was built against",
    ));
    assert!(
        header.is_file(),
        "APPLICATION_ENGINE_HEADER points at {}, which is not a file",
        header.display()
    );

    let declared = env::var("APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT").expect(
        "APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT is unset, set it to the application's largest \
         method payload, the same value the archive was built with",
    );
    let limit = declared.trim().parse::<u32>().unwrap_or_else(|err| {
        panic!("APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT is not a number: {err}")
    });
    // The bound gates ingress and sizes batches, so a fat-fingered value is worth refusing here
    // rather than discovering as a memory bill
    assert!(
        limit > 0 && limit <= MAX_METHOD_PAYLOAD_LIMIT,
        "APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT is {limit}, expected 1..={MAX_METHOD_PAYLOAD_LIMIT}"
    );
    (header, limit)
}

fn main() {
    println!("cargo::rerun-if-env-changed=APPLICATION_ENGINE_LIB");
    println!("cargo::rerun-if-env-changed=APPLICATION_ENGINE_HEADER");
    println!("cargo::rerun-if-env-changed=APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let (header, method_payload_limit) = match env::var("APPLICATION_ENGINE_LIB") {
        Ok(engine_lib) => external_engine(&engine_lib),
        // Linked from inside this workspace, where `c-wallet-engine` is the engine
        Err(_) => (
            manifest_dir.join("include").join("application-engine.h"),
            REFERENCE_ENGINE_METHOD_PAYLOAD_LIMIT,
        ),
    };

    println!("cargo::rerun-if-changed={}", header.display());
    generate_bindings(&header, method_payload_limit);
}
