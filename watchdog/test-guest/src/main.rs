// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Cartesi Machine test guest for the watchdog. It keeps a small layout in the
//! `state` NVRAM and drives every rollup host outcome on request: accept,
//! reject, exception, halt, advance reports and inspect. README.md has the
//! input protocol and the state layout.

use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use trolley::cmt::RollupCmt;
use trolley::{Rollup, RollupRequest};

const STATE_LABEL: &str = "state";
const STATE_LEN: usize = 64 * 1024;
const COUNT: usize = 0;
const TOTAL: usize = 8;
const LAST_LEN: usize = 16;
const LAST: usize = 20;
const MAX_LAST_LEN: usize = 4096;
/// Bytes a `reject` input overwrites before rejecting, so a host that fails
/// to restore the pre-input snapshot is caught.
const REJECT_POISON_LEN: usize = 64;
const HALT_EXIT_CODE: i32 = 7;
const EXCEPTION_MESSAGE: &[u8] = b"test-guest exception";

fn main() {
    let state = map_nvram(STATE_LABEL, STATE_LEN);
    // Written before the first accept yield, so the stored template holds it.
    state.fill(0);

    let mut rollup = RollupCmt::try_new().expect("failed to initialize rollup");
    loop {
        match rollup
            .next_input()
            .expect("failed to receive the next request")
        {
            RollupRequest::Advance { payload, .. } => match payload.as_slice() {
                b"reject" => {
                    state[..REJECT_POISON_LEN].fill(0xff);
                    rollup.revert();
                }
                b"exception" => raise_exception(rollup, EXCEPTION_MESSAGE),
                b"halt" => std::process::exit(HALT_EXIT_CODE),
                payload => {
                    if payload.starts_with(b"report") {
                        rollup.emit_report(payload).expect("failed to emit report");
                    }
                    record_accepted(state, payload);
                }
            },
            RollupRequest::Inspect { payload } => {
                let report: &[u8] = if payload == b"state" {
                    state
                } else {
                    b"unsupported"
                };
                rollup.emit_report(report).expect("failed to emit report");
            }
        }
    }
}

/// Little-endian layout: accepted-input count (u64) at 0, total accepted
/// payload bytes (u64) at 8, length L of the last accepted payload (u32,
/// capped at 4096) at 16, its first L bytes at 20, zeros everywhere else.
fn record_accepted(state: &mut [u8], payload: &[u8]) {
    let count = read_u64(state, COUNT)
        .checked_add(1)
        .expect("accepted-input count overflow");
    let total = read_u64(state, TOTAL)
        .checked_add(payload.len() as u64)
        .expect("accepted payload total overflow");
    let last = &payload[..payload.len().min(MAX_LAST_LEN)];

    state[COUNT..COUNT + 8].copy_from_slice(&count.to_le_bytes());
    state[TOTAL..TOTAL + 8].copy_from_slice(&total.to_le_bytes());
    state[LAST_LEN..LAST_LEN + 4].copy_from_slice(&(last.len() as u32).to_le_bytes());
    let region = &mut state[LAST..LAST + MAX_LAST_LEN];
    region[..last.len()].copy_from_slice(last);
    // A shorter payload must not leave the previous payload's tail behind.
    region[last.len()..].fill(0);
}

fn read_u64(state: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(state[offset..offset + 8].try_into().unwrap())
}

/// Maps the NVRAM with user label `label` shared and read-write. NVRAM has no
/// page cache, so stores are machine state at once and nothing is flushed.
/// The device is resolved as the guest-tools `labelinfo` does: the device-tree
/// alias names the `/uio@<start>` node, whose `reg` holds start and length,
/// and the platform device `<start>.uio` owns exactly one uio device.
fn map_nvram(label: &str, len: usize) -> &'static mut [u8] {
    let alias = fs::read(format!("/proc/device-tree/aliases/{label}"))
        .unwrap_or_else(|e| panic!("no device-tree alias for label {label:?}: {e}"));
    let node = std::str::from_utf8(alias.strip_suffix(b"\0").unwrap_or(&alias))
        .expect("device-tree alias is not UTF-8");
    assert!(
        node.starts_with("/uio@"),
        "label {label:?} is not an NVRAM: {node}"
    );

    let reg = fs::read(format!("/proc/device-tree{node}/reg"))
        .unwrap_or_else(|e| panic!("cannot read reg of {node}: {e}"));
    assert_eq!(reg.len(), 16, "reg of {node} is not (start, length)");
    let start = u64::from_be_bytes(reg[..8].try_into().unwrap());
    let length = u64::from_be_bytes(reg[8..].try_into().unwrap());
    assert_eq!(length, len as u64, "length of NVRAM {label:?}");

    let uio_dir = format!("/sys/devices/platform/{start:x}.uio/uio");
    let devices: Vec<_> = fs::read_dir(&uio_dir)
        .unwrap_or_else(|e| panic!("cannot list {uio_dir}: {e}"))
        .map(|entry| entry.expect("cannot read uio entry").file_name())
        .collect();
    let [device] = devices.as_slice() else {
        panic!("expected one uio device in {uio_dir}, found {devices:?}");
    };
    let device = PathBuf::from("/dev").join(device);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", device.display()));
    // SAFETY: a fresh shared mapping of the device's map0 (offset 0). It is
    // never unmapped and nothing else in this process aliases it; it stays
    // valid after the descriptor closes.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        ptr,
        libc::MAP_FAILED,
        "cannot mmap {}: {}",
        device.display(),
        std::io::Error::last_os_error()
    );
    // SAFETY: `ptr` maps `len` writable bytes for the rest of the process.
    unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) }
}

/// Raises a CMIO exception (a `TX_EXCEPTION` manual yield). trolley has no
/// exception call and keeps its libcmt handle private, so this closes that
/// handle and raises the exception through a fresh one. An exception is a
/// fixed point: a conforming host never resumes the machine.
fn raise_exception(rollup: RollupCmt, message: &[u8]) -> ! {
    drop(rollup);
    let mut cmt = MaybeUninit::<libcmt_sys::cmt_rollup_t>::uninit();
    // SAFETY: cmt_rollup_init fully initializes `cmt` when it returns 0.
    let rc = unsafe { libcmt_sys::cmt_rollup_init(cmt.as_mut_ptr()) };
    assert_eq!(rc, 0, "cmt_rollup_init failed");
    let mut cmt = unsafe { cmt.assume_init() };
    let payload = libcmt_sys::cmt_abi_bytes_t {
        data: message.as_ptr() as *mut c_void,
        length: message.len(),
    };
    // SAFETY: `payload` borrows `message`, which libcmt only reads.
    let rc = unsafe { libcmt_sys::cmt_rollup_emit_exception(&mut cmt, &payload) };
    panic!("host resumed the machine after an exception (rc={rc})");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(count: u64, total: u64, last: &[u8]) -> Vec<u8> {
        let mut state = vec![0; STATE_LEN];
        state[0..8].copy_from_slice(&count.to_le_bytes());
        state[8..16].copy_from_slice(&total.to_le_bytes());
        state[16..20].copy_from_slice(&(last.len() as u32).to_le_bytes());
        state[20..20 + last.len()].copy_from_slice(last);
        state
    }

    #[test]
    fn shorter_payload_clears_previous_tail() {
        let mut state = vec![0; STATE_LEN];
        record_accepted(&mut state, b"hello world");
        record_accepted(&mut state, b"hi");
        assert_eq!(state, layout(2, 13, b"hi"));
    }

    #[test]
    fn last_payload_is_capped_but_total_counts_all_bytes() {
        let mut state = vec![0; STATE_LEN];
        let payload = vec![0xab; MAX_LAST_LEN + 10];
        record_accepted(&mut state, &payload);
        assert_eq!(
            state,
            layout(1, payload.len() as u64, &payload[..MAX_LAST_LEN])
        );
    }

    #[test]
    fn empty_payload_is_accepted() {
        let mut state = vec![0; STATE_LEN];
        record_accepted(&mut state, b"abc");
        record_accepted(&mut state, b"");
        assert_eq!(state, layout(2, 3, b""));
    }
}
