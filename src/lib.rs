pub mod decoder;

use std::ffi::CStr;

use media_codec_vpx_sys as vpx_sys;

pub(crate) fn vpx_error_string(error: vpx_sys::vpx_codec_err_t) -> String {
    unsafe { CStr::from_ptr(vpx_sys::vpx_codec_err_to_string(error)).to_string_lossy().into_owned() }
}
