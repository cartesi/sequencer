use crate::{InputMetadata, RollupError, RollupRequest, RollupResult};

use core::ffi::c_void;
use rollups_types::alloy_primitives::U256;
use std::mem::MaybeUninit;

pub struct RollupCmt {
    r: libcmt_sys::cmt_rollup_t,
}

impl RollupCmt {
    pub fn try_new() -> RollupResult<Self> {
        let mut r: MaybeUninit<libcmt_sys::cmt_rollup_t> = MaybeUninit::uninit();
        let rc = unsafe { libcmt_sys::cmt_rollup_init(r.as_mut_ptr()) };
        cmt_ok("cmt_rollup_init", rc)?;
        Ok(Self {
            r: unsafe { r.assume_init() },
        })
    }

    pub fn new() -> Self {
        Self::try_new().expect("failed to initialize rollup from libcmt")
    }
}

impl Default for RollupCmt {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::Rollup for RollupCmt {
    fn next_input(&mut self) -> RollupResult<RollupRequest> {
        let mut finish = libcmt_sys::cmt_rollup_finish_t {
            accept_previous_request: true,
            next_request_type: 0,
            next_request_payload_length: 0,
        };

        let rc = unsafe { libcmt_sys::cmt_rollup_finish(&mut self.r, &mut finish) };
        cmt_ok("cmt_rollup_finish(accept_previous_request=true)", rc)?;

        let request_type = finish.next_request_type as u32;
        match request_type {
            x if x == libcmt_sys::HTIF_YIELD_REASON_ADVANCE_STATE => {
                let mut advance: MaybeUninit<libcmt_sys::cmt_rollup_advance_t> =
                    MaybeUninit::uninit();
                let rc = unsafe {
                    libcmt_sys::cmt_rollup_read_advance_state(&mut self.r, advance.as_mut_ptr())
                };
                cmt_ok("cmt_rollup_read_advance_state", rc)?;
                let advance = unsafe { advance.assume_init() };

                let metadata = InputMetadata {
                    chain_id: U256::from(advance.chain_id),
                    app_contract: advance.app_contract.data.into(),
                    msg_sender: advance.msg_sender.data.into(),
                    block_number: U256::from(advance.block_number),
                    block_timestamp: U256::from(advance.block_timestamp),
                    prev_randao: U256::from_be_bytes(advance.prev_randao.data),
                    index: U256::from(advance.index),
                };
                let payload = payload_from_raw(
                    "advance.payload",
                    advance.payload.data,
                    advance.payload.length,
                )?;
                Ok(RollupRequest::Advance { metadata, payload })
            }
            x if x == libcmt_sys::HTIF_YIELD_REASON_INSPECT_STATE => {
                let mut inspect: MaybeUninit<libcmt_sys::cmt_rollup_inspect_t> =
                    MaybeUninit::uninit();
                let rc = unsafe {
                    libcmt_sys::cmt_rollup_read_inspect_state(&mut self.r, inspect.as_mut_ptr())
                };
                cmt_ok("cmt_rollup_read_inspect_state", rc)?;
                let inspect = unsafe { inspect.assume_init() };
                let payload = payload_from_raw(
                    "inspect.payload",
                    inspect.payload.data,
                    inspect.payload.length,
                )?;
                Ok(RollupRequest::Inspect { payload })
            }
            _ => Err(RollupError::UnexpectedRequestType { request_type }),
        }
    }

    fn revert(&mut self) -> ! {
        let mut finish = libcmt_sys::cmt_rollup_finish_t {
            accept_previous_request: false,
            next_request_type: 0,
            next_request_payload_length: 0,
        };

        let rc = unsafe { libcmt_sys::cmt_rollup_finish(&mut self.r, &mut finish) };
        if rc != 0 {
            panic!("cmt_rollup_finish(accept_previous_request=false) failed with rc={rc}");
        }
        panic!("revert finished unexpectedly; expected libcmt to halt execution");
    }

    fn gio(&mut self, domain: u16, id: &[u8]) -> RollupResult<(u16, Vec<u8>)> {
        if id.len() > u32::MAX as usize {
            return Err(RollupError::LengthOverflow {
                field: "gio.id",
                len: id.len(),
                max: u32::MAX as usize,
            });
        }

        let mut req = libcmt_sys::cmt_gio {
            domain,
            id_length: id.len() as u32,
            id: id.as_ptr() as *mut c_void,
            response_code: 0,
            response_data_length: 0,
            response_data: std::ptr::null_mut(),
        };

        let rc = unsafe { libcmt_sys::cmt_gio_request(&mut self.r, &mut req) };
        cmt_ok("cmt_gio_request", rc)?;

        let response = payload_from_raw(
            "gio.response_data",
            req.response_data,
            req.response_data_length as usize,
        )?;
        Ok((req.response_code, response))
    }

    fn emit_voucher(&mut self, voucher: &rollups_types::Voucher) -> RollupResult<()> {
        let destination = voucher.destination;
        let value = voucher.value.to_be_bytes();
        let mut index = 0;

        let rc = unsafe {
            libcmt_sys::cmt_rollup_emit_voucher(
                &mut self.r,
                &libcmt_sys::cmt_abi_address {
                    data: **destination,
                },
                &libcmt_sys::cmt_abi_u256 { data: value },
                &libcmt_sys::cmt_abi_bytes_t {
                    data: voucher.payload.as_ptr() as *mut c_void,
                    length: voucher.payload.len(),
                },
                &mut index,
            )
        };
        cmt_ok("cmt_rollup_emit_voucher", rc)
    }

    fn emit_notice(&mut self, notice: &rollups_types::Notice) -> RollupResult<()> {
        let mut index = 0;
        let rc = unsafe {
            libcmt_sys::cmt_rollup_emit_notice(
                &mut self.r,
                &libcmt_sys::cmt_abi_bytes_t {
                    data: notice.payload.as_ptr() as *mut c_void,
                    length: notice.payload.len(),
                },
                &mut index,
            )
        };
        cmt_ok("cmt_rollup_emit_notice", rc)
    }

    fn emit_report(&mut self, report: &[u8]) -> RollupResult<()> {
        let rc = unsafe {
            libcmt_sys::cmt_rollup_emit_report(
                &mut self.r,
                &libcmt_sys::cmt_abi_bytes_t {
                    data: report.as_ptr() as *mut c_void,
                    length: report.len(),
                },
            )
        };
        cmt_ok("cmt_rollup_emit_report", rc)
    }
}

impl Drop for RollupCmt {
    fn drop(&mut self) {
        unsafe { libcmt_sys::cmt_rollup_fini(&mut self.r) }
    }
}

fn cmt_ok(operation: &'static str, rc: i32) -> RollupResult<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(RollupError::CmtCallFailed {
            operation,
            code: rc,
        })
    }
}

fn payload_from_raw(
    field: &'static str,
    data: *mut c_void,
    length: usize,
) -> RollupResult<Vec<u8>> {
    if length == 0 {
        return Ok(Vec::new());
    }
    if data.is_null() {
        return Err(RollupError::InvalidPayloadPointer { field, len: length });
    }
    Ok(unsafe { std::slice::from_raw_parts(data as *const u8, length) }.to_vec())
}
