//! Checking a password with PAM (Linux-PAM), the way login does.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_REFRESH_CRED: c_int = 0x10;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        pamh: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
}

unsafe extern "C" {
    fn calloc(n: usize, size: usize) -> *mut c_void;
    fn strdup(s: *const c_char) -> *mut c_char;
    fn free(p: *mut c_void);
}

/// Answers every hidden or visible prompt with the password (PAM frees
/// what we hand it).
unsafe extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    let Ok(count) = usize::try_from(num_msg) else {
        return PAM_CONV_ERR;
    };
    if count == 0 || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: PAM passes `count` messages (Linux-PAM: an array of pointers)
    // and owns the responses we calloc; `appdata` is the CString we gave.
    unsafe {
        let password = appdata as *const c_char;
        let replies = calloc(count, size_of::<PamResponse>()) as *mut PamResponse;
        if replies.is_null() {
            return PAM_BUF_ERR;
        }
        for i in 0..count {
            let message = *msg.add(i);
            if message.is_null() {
                continue;
            }
            match (*message).msg_style {
                PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                    let copy = strdup(password);
                    if copy.is_null() {
                        for j in 0..i {
                            let reply = replies.add(j);
                            if !(*reply).resp.is_null() {
                                free((*reply).resp as *mut c_void);
                            }
                        }
                        free(replies as *mut c_void);
                        return PAM_BUF_ERR;
                    }
                    (*replies.add(i)).resp = copy;
                }
                PAM_ERROR_MSG | PAM_TEXT_INFO => {}
                _ => {
                    free(replies as *mut c_void);
                    return PAM_CONV_ERR;
                }
            }
        }
        *resp = replies;
    }
    PAM_SUCCESS
}

/// Whether `password` is `user`'s, by PAM `service`. Blocks (a wrong
/// password usually takes a couple of seconds); run it off the UI thread.
pub fn authenticate(service: &str, user: &str, password: &[u8]) -> Result<(), String> {
    let service = CString::new(service).map_err(|_| "bad service name")?;
    let user = CString::new(user).map_err(|_| "bad user name")?;
    let password = CString::new(password).map_err(|_| "The password can't contain a NUL")?;
    let conv = PamConv {
        conv: Some(converse),
        appdata_ptr: password.as_ptr() as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();
    // SAFETY: every pointer outlives the PAM handle, which is ended below.
    let result = unsafe {
        let started = pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle);
        if started != PAM_SUCCESS {
            return Err(format!("PAM wouldn't start ({started})"));
        }
        let status = pam_authenticate(handle, 0);
        let result = if status == PAM_SUCCESS {
            // Refresh Kerberos tickets and the like; failing that is fine.
            pam_setcred(handle, PAM_REFRESH_CRED);
            Ok(())
        } else {
            let text = pam_strerror(handle, status);
            Err(if text.is_null() {
                format!("PAM error {status}")
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            })
        };
        pam_end(handle, status);
        result
    };
    // Don't leave the password lying around in freed memory.
    let mut bytes = password.into_bytes_with_nul();
    for b in &mut bytes {
        // SAFETY: a plain write to memory we own; volatile so it isn't
        // optimised away.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    result
}
