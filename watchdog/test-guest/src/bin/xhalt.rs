// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Musl build of the guest-tools `xhalt`, whose upstream binary is linked
//! against glibc and cannot run on the Alpine rootfs. When the entrypoint
//! exits non-zero, cartesi-init runs `/usr/sbin/xhalt <status>` as root; the
//! Cartesi kernel turns the `LINUX_REBOOT_CMD_RESTART2` argument into the
//! machine's halt exit code. Without it the machine halts with code 0.

#[cfg(target_os = "linux")]
fn main() {
    let status = std::env::args().nth(1).unwrap_or_else(|| "0".to_owned());
    let status = std::ffi::CString::new(status).expect("status contains a NUL byte");
    // SAFETY: reboot(2) only reads the NUL-terminated argument string.
    unsafe {
        libc::syscall(
            libc::SYS_reboot,
            libc::LINUX_REBOOT_MAGIC1,
            libc::LINUX_REBOOT_MAGIC2,
            libc::LINUX_REBOOT_CMD_RESTART2,
            status.as_ptr(),
        );
    }
    eprintln!("xhalt: reboot failed: {}", std::io::Error::last_os_error());
    std::process::exit(1);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("xhalt only runs inside the Cartesi Machine");
    std::process::exit(1);
}
