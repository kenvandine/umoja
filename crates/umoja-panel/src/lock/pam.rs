//! Minimal PAM client for the lock screen: check the user's password with
//! the `umoja-lock` service (common-auth + common-account), the way
//! gnome-shell's unlock dialog does through gdm-password.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

const SERVICE: &str = "umoja-lock";

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_CONV_ERR: c_int = 19;
const PAM_BUF_ERR: c_int = 5;
const PAM_REFRESH_CRED: c_int = 0x0010;

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
    conv: ConvFn,
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
    fn pam_acct_mgmt(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
}

/// Answers every prompt with the password; info/error messages get no reply.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    // PAM frees the array and each `resp` string with free().
    let replies =
        unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    let password = appdata as *const c_char;
    for i in 0..n {
        // Linux-PAM passes an array of pointers.
        let m = unsafe { *msg.add(i) };
        if m.is_null() {
            continue;
        }
        let style = unsafe { (*m).msg_style };
        if style == PAM_PROMPT_ECHO_OFF || style == PAM_PROMPT_ECHO_ON {
            unsafe { (*replies.add(i)).resp = libc::strdup(password) };
        }
    }
    unsafe { *resp = replies };
    PAM_SUCCESS
}

/// Blocking; call it off the main thread. `Err` carries PAM's message.
pub fn authenticate(user: &str, password: &str) -> Result<(), String> {
    let service = CString::new(SERVICE).expect("no NUL");
    let user = CString::new(user).map_err(|e| e.to_string())?;
    let password = CString::new(password).map_err(|e| e.to_string())?;
    let conv = PamConv {
        conv: conversation,
        appdata_ptr: password.as_ptr() as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
    if rc != PAM_SUCCESS || handle.is_null() {
        return Err(format!("pam_start failed ({rc})"));
    }
    let error = |rc: c_int| unsafe {
        let s = pam_strerror(handle, rc);
        if s.is_null() {
            format!("PAM error {rc}")
        } else {
            CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    };
    let mut rc = unsafe { pam_authenticate(handle, 0) };
    let mut result = if rc == PAM_SUCCESS {
        Ok(())
    } else {
        Err(error(rc))
    };
    if result.is_ok() {
        rc = unsafe { pam_acct_mgmt(handle, 0) };
        if rc != PAM_SUCCESS {
            result = Err(error(rc));
        } else {
            // Refresh Kerberos tickets etc., like gnome-screensaver did.
            unsafe { pam_setcred(handle, PAM_REFRESH_CRED) };
        }
    }
    unsafe { pam_end(handle, rc) };
    // Don't leave the password lying around in freed memory.
    let mut bytes = password.into_bytes_with_nul();
    for b in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    result
}

/// The `umoja-lock` account checks alone (expired or locked account...),
/// for an unlock that was authenticated by fingerprint. Blocking.
pub fn account_ok(user: &str) -> Result<(), String> {
    let service = CString::new(SERVICE).expect("no NUL");
    let user = CString::new(user).map_err(|e| e.to_string())?;
    // Any prompt (there should be none) gets an empty answer.
    let empty = CString::default();
    let conv = PamConv {
        conv: conversation,
        appdata_ptr: empty.as_ptr() as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
    if rc != PAM_SUCCESS || handle.is_null() {
        return Err(format!("pam_start failed ({rc})"));
    }
    let rc = unsafe { pam_acct_mgmt(handle, 0) };
    let result = if rc == PAM_SUCCESS {
        Ok(())
    } else {
        Err(unsafe {
            let s = pam_strerror(handle, rc);
            if s.is_null() {
                format!("PAM error {rc}")
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        })
    };
    unsafe { pam_end(handle, rc) };
    result
}
