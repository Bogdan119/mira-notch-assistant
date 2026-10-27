#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod anim;
#[cfg(windows)]
mod com;
#[cfg(windows)]
mod files;
#[cfg(windows)]
mod gfx;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod pet;
#[cfg(windows)]
mod phone;
#[cfg(windows)]
mod sensor;

/// NUL-terminated UTF-16 for Win32 string parameters.
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
fn main() {
    // All Win32 objects are owned and accessed by the message-loop thread.
    unsafe { host::run() }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mira-win requires Windows 10 or 11.");
}
