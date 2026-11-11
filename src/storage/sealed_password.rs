//! macOS saved passwords sealed to a Secure Enclave key (see
//! macos/sealed_password.swift). Opening one asks for Touch ID, or for the Mac
//! password when Touch ID is unavailable, instead of the login keychain
//! password that every rebuilt binary would otherwise need.
use std::ffi::{CStr, CString, c_char, c_void};

unsafe extern "C" {
    fn snack_seal_password(input: *const u8, input_length: usize, output: *mut *mut u8,
        output_length: *mut usize, error: *mut *mut c_char) -> i32;
    fn snack_open_password(input: *const u8, input_length: usize, reason: *const c_char, output: *mut *mut u8,
        output_length: *mut usize, error: *mut *mut c_char) -> i32;
    fn snack_free(pointer: *mut c_void);
}

#[derive(Debug, PartialEq)]
pub enum OpenError {
    Cancelled,
    Failed(String),
}

// Take ownership of a buffer and message allocated by the helper.
unsafe fn take(output: *mut u8, length: usize, error: *mut c_char) -> (Vec<u8>, String) {
    let bytes = if output.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(output, length) }.to_vec() };
    let message = if error.is_null() { String::new() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned() };
    unsafe {
        if !output.is_null() {
            // Clear secrets before returning the buffer to the allocator.
            std::ptr::write_bytes(output, 0, length);
            snack_free(output.cast());
        }
        snack_free(error.cast());
    }
    (bytes, message)
}

pub fn seal(password: &str) -> Result<Vec<u8>, String> {
    let (mut output, mut length, mut error) = (std::ptr::null_mut(), 0, std::ptr::null_mut());
    let status = unsafe { snack_seal_password(password.as_ptr(), password.len(), &mut output, &mut length, &mut error) };
    let (sealed, message) = unsafe { take(output, length, error) };
    if status == 0 { Ok(sealed) } else { Err(format!("Could not protect the saved password: {message}")) }
}

pub fn open(sealed: &[u8], reason: &str) -> Result<String, OpenError> {
    let reason = CString::new(reason).unwrap_or_default();
    let (mut output, mut length, mut error) = (std::ptr::null_mut(), 0, std::ptr::null_mut());
    let status = unsafe { snack_open_password(sealed.as_ptr(), sealed.len(), reason.as_ptr(), &mut output, &mut length, &mut error) };
    let (password, message) = unsafe { take(output, length, error) };
    match status {
        0 => String::from_utf8(password).map_err(|_| OpenError::Failed("The saved password is not valid text.".into())),
        1 => Err(OpenError::Cancelled),
        _ => Err(OpenError::Failed(message)),
    }
}
