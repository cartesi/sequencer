use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

// Vendored libcmt sources (machine-guest-tools v0.18.0, see ../README.md).
// libcmt 0.18 bundles its cmio ioctl definitions (`libcmt/ioctl.h`), so the
// riscv64 build no longer needs the Cartesi Linux kernel headers.
const LIBCMT_PATH: &str = "libcmt";

fn main() {
    let target = env::var("TARGET").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let is_riscv_target = target.starts_with("riscv64");

    // 1. Prepare Source (Sandbox)
    let original_src = manifest_dir.join(LIBCMT_PATH);
    let build_dir = out_dir.join("libcmt_build");

    // Always clean and recopy to ensure no stale artifacts
    if build_dir.exists() {
        std::fs::remove_dir_all(&build_dir).unwrap();
    }
    copy_dir_recursive(&original_src, &build_dir).expect("Failed to copy source");

    let headers_path = build_dir.join("include/libcmt");

    // 2. Build with Make
    let lib_output_dir = if is_riscv_target {
        let status = Command::new("make")
            .current_dir(&build_dir)
            .env("TOOLCHAIN_PREFIX", "riscv64-unknown-linux-musl-")
            .status()
            .expect("Failed to execute make");

        if !status.success() {
            panic!("libcmt Makefile failed");
        }

        build_dir.join("build/riscv64")
    } else {
        println!("Skipping real build on host; building mock");

        // Mock build for Host / LSP
        let status = Command::new("make")
            .current_dir(&build_dir)
            .arg("mock")
            .status()
            .expect("Failed to execute make");

        if !status.success() {
            panic!("libcmt Makefile mock failed");
        }

        build_dir.join("build/mock")
    };

    // 3. Generate Bindings
    let mut bindgen = bindgen::Builder::default()
        .header(headers_path.join("rollup.h").to_str().unwrap())
        .use_core()
        .ctypes_prefix("core::ffi");
    if is_riscv_target {
        bindgen = bindgen.clang_arg("--target=riscv64-unknown-linux-musl");
    }
    let bindings = bindgen.generate().expect("Unable to generate bindings");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // 4. Link
    // Sanity check to save hours of debugging
    if !lib_output_dir.join("libcmt.a").exists() {
        panic!(
            "Build succeeded but libcmt.a not found at expected path: {}",
            lib_output_dir.display()
        );
    }

    println!(
        "cargo:rustc-link-search=native={}",
        lib_output_dir.display()
    );
    println!("cargo:rustc-link-lib=static=cmt");
    println!("cargo:rerun-if-changed={}", LIBCMT_PATH);
    println!("cargo:rerun-if-changed=build.rs");
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
