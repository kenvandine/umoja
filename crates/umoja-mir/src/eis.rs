//! libei connections for remote desktop sessions (ConnectToEIS); the EIS
//! side itself is shim/eis.cpp. Input capture uses its own (input_capture.rs).

use std::os::fd::{FromRawFd, OwnedFd as StdOwnedFd};
use zbus::zvariant::OwnedFd;

#[repr(C)]
struct Region {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: f64,
}

unsafe extern "C" {
    fn umoja_eis_connect_remote(
        remote: u64,
        regions: *const Region,
        n: usize,
        out_fd: *mut i32,
    ) -> u64;
    fn umoja_eis_disconnect(handle: u64);
}

/// A libei connection feeding remote input handle `remote`: (handle, the
/// client's end). Absolute positions cover the monitors, in layout
/// coordinates.
pub fn connect(remote: u64) -> Result<(u64, OwnedFd), String> {
    let regions: Vec<Region> = crate::display_config::outputs()
        .values()
        .map(|o| Region {
            x: o.rect.x,
            y: o.rect.y,
            w: o.rect.w,
            h: o.rect.h,
            scale: o.scale,
        })
        .collect();
    let mut fd = -1;
    let handle =
        unsafe { umoja_eis_connect_remote(remote, regions.as_ptr(), regions.len(), &mut fd) };
    if handle == 0 || fd < 0 {
        return Err("could not set up a libei connection".into());
    }
    // SAFETY: the shim hands us the client's end, which nothing else owns.
    let fd = unsafe { StdOwnedFd::from_raw_fd(fd) };
    Ok((handle, OwnedFd::from(fd)))
}

/// Close a connection (blocks until its thread is done).
pub fn disconnect(handle: u64) {
    unsafe { umoja_eis_disconnect(handle) };
}
