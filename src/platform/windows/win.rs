//! Small helpers for calling Win32 APIs.

use std::ffi::CStr;
use std::io;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// Encodes a string as NUL-terminated UTF-16 for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The calling thread's last Win32 error, as text.
pub fn last_error() -> String {
    io::Error::last_os_error().to_string()
}

/// Looks up an export of a system DLL at run time, for functions that older
/// Windows versions may lack. `F` must be the function's pointer type.
pub fn system_proc<F: Copy>(dll: &str, name: &CStr) -> Option<F> {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    let dll = wide(dll);
    // SAFETY: loading a system DLL by name and looking up an export; the
    // caller supplies the export's real signature as F.
    unsafe {
        let module = LoadLibraryW(dll.as_ptr());
        if module.is_null() {
            return None;
        }
        let proc = GetProcAddress(module, name.as_ptr().cast())?;
        Some(std::mem::transmute_copy(&proc))
    }
}
