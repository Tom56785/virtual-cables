//! Small helpers for calling Win32 APIs.

use std::ffi::CStr;
use std::io;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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

/// Whether this process has administrator rights: run as administrator, or
/// as the service.
pub fn is_elevated() -> bool {
    // SAFETY: queries this process's own token into a local structure, then
    // closes the token.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// A security descriptor made from an SDDL string, freed on drop.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is memory this value owns, which Windows only reads.
unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn new(sddl: &str) -> Result<SecurityDescriptor, String> {
        let sddl = wide(sddl);
        let mut descriptor = null_mut();
        // SAFETY: converts a NUL-terminated string; the result is freed on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!("security descriptor: {}", last_error()));
        }
        Ok(SecurityDescriptor(descriptor))
    }

    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_descriptors_parse() {
        assert!(SecurityDescriptor::new("D:P(A;;GA;;;SY)(A;;GRGW;;;IU)").is_ok());
        assert!(SecurityDescriptor::new("not SDDL").is_err());
    }
}
