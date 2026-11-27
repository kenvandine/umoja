//! CAP_SYS_NICE, for high-priority GPU contexts.
//!
//! Mir asks for high-priority EGL contexts (Umoja's Mir patch 3030) so the
//! desktop keeps drawing while a game fills the GPU. amdgpu and i915 grant
//! that only to the DRM master or a process with CAP_SYS_NICE, and Mir draws
//! on the render node, so the package gives umoja-mir the capability
//! (debian/umoja-desktop.postinst).
//!
//! It is set as permitted only (`cap_sys_nice=p`), without the file's
//! effective bit: with that bit the kernel refuses to run the binary at all
//! where the capability can't be had (a container's bounding set, or
//! NoNewPrivileges), so the compositor wouldn't start. umoja-mir raises it
//! into its effective set itself, when it was granted.

/// Raises CAP_SYS_NICE into the effective set if it is permitted. Returns
/// whether it is now effective.
///
/// Capabilities belong to threads, and new threads copy the creating
/// thread's, so this must run on the main thread before Mir starts any.
pub fn raise_sys_nice() -> bool {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    // _LINUX_CAPABILITY_VERSION_3: two Data words, capabilities 0-63
    const VERSION_3: u32 = 0x2008_0522;
    const CAP_SYS_NICE: u32 = 1 << 23;

    let mut header = Header { version: VERSION_3, pid: 0 };
    let mut data = [Data::default(); 2];
    // SAFETY: capget writes one header and two Data words, which is what we
    // pass for version 3.
    let got = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut Header,
            data.as_mut_ptr(),
        )
    };
    if got != 0 || data[0].permitted & CAP_SYS_NICE == 0 {
        return false;
    }
    if data[0].effective & CAP_SYS_NICE != 0 {
        return true;
    }
    data[0].effective |= CAP_SYS_NICE;
    // SAFETY: as above; it only adds a permitted capability to the effective
    // set of this thread.
    unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut Header,
            data.as_ptr(),
        ) == 0
    }
}
