//! Authentication of both ends of the helper pipe (DESIGN s5.3.1).
//!
//! Two independent, mutually-distrusting checks:
//! - The **helper** restricts the pipe's DACL to the creating user + local
//!   Administrators ([`pipe_sddl`]) and, per connection, requires the connecting
//!   client's process image to live in the helper's own install directory
//!   ([`is_sibling_image`]) - so no other user, and no unrelated same-user
//!   process, can drive it.
//! - The **client** verifies the server end is the expected helper image in the
//!   client's own install directory before sending any path, so a rogue pipe
//!   squatter cannot impersonate the helper.
//!
//! The SDDL construction and the image-directory comparison are pure and
//! unit-tested cross-OS. The SID lookup, the security-descriptor allocation, and
//! the pipe/process-image syscalls are Windows-gated.

use std::path::Path;

/// Build the SDDL string for the pipe's security descriptor: a PROTECTED DACL
/// (`D:P`, no inherited ACEs) granting GENERIC_ALL to `user_sid` and to
/// `BUILTIN\Administrators` (`BA`), and - by having no other ACE - denying
/// everyone else.
///
/// `user_sid` must be a valid string SID (`S-1-5-21-...`). A malformed SID
/// yields a descriptor Windows will reject at parse time, which the caller
/// surfaces as a launch failure (fail-closed).
pub fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})(A;;GA;;;BA)")
}

/// The directory portion of an executable path as a lowercased, `\`-joined key
/// for case-insensitive comparison.
///
/// Treats BOTH `\` and `/` as separators (Rust's `Path::parent` is
/// platform-separator-specific, and the helper only handles Windows paths), so
/// this behaves identically on the Windows helper and in the cross-OS unit
/// tests. A bare filename with no directory yields `None` so two directory-less
/// images never count as siblings (fail-closed).
fn image_dir_key(exe: &Path) -> Option<String> {
    let s = exe.to_string_lossy();
    let s = s
        .strip_prefix(r"\\?\")
        .or_else(|| s.strip_prefix(r"\\.\"))
        .unwrap_or(&s);
    let segs: Vec<&str> = s.split(['/', '\\']).filter(|seg| !seg.is_empty()).collect();
    if segs.len() < 2 {
        // No directory component (bare filename): un-verifiable.
        return None;
    }
    Some(segs[..segs.len() - 1].join("\\").to_ascii_lowercase())
}

/// `true` when `other` is an executable in the SAME install directory as
/// `reference` (both the helper and the app ship in one directory). This is the
/// image-identity check both ends run against the other's resolved process
/// image path.
///
/// A future hardening (documented in DESIGN s5.3.1) adds Authenticode signature
/// verification on top; same-directory is the V1.x floor.
pub fn is_sibling_image(reference: &Path, other: &Path) -> bool {
    match (image_dir_key(reference), image_dir_key(other)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The Windows syscall layer: SID lookup, security-descriptor allocation, and
/// pipe/process-image resolution.
#[cfg(windows)]
pub mod windows_impl {
    use std::path::PathBuf;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// The current process user's SID as an `S-1-...` string.
    pub fn current_user_sid_string() -> Result<String, String> {
        // SAFETY: standard OpenProcessToken + GetTokenInformation(TokenUser)
        // dance; the token handle is closed on every path, and the SID string
        // that ConvertSidToStringSidW allocates is freed with LocalFree.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|e| format!("OpenProcessToken: {e}"))?;

            // First call sizes the buffer.
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                let _ = CloseHandle(token);
                return Err("GetTokenInformation returned zero size".to_string());
            }
            let mut buf = vec![0u8; needed as usize];
            let res = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                needed,
                &mut needed,
            );
            let _ = CloseHandle(token);
            res.map_err(|e| format!("GetTokenInformation: {e}"))?;

            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut str_sid = PWSTR::null();
            ConvertSidToStringSidW(token_user.User.Sid, &mut str_sid)
                .map_err(|e| format!("ConvertSidToStringSid: {e}"))?;
            if str_sid.is_null() {
                return Err("ConvertSidToStringSid produced null".to_string());
            }
            let s = str_sid.to_string().map_err(|e| format!("SID utf16: {e}"))?;
            let _ = LocalFree(Some(HLOCAL(str_sid.as_ptr() as *mut core::ffi::c_void)));
            Ok(s)
        }
    }

    /// An owned security descriptor built from an SDDL string, freeing its
    /// LocalAlloc'd backing on drop. Hand [`Self::as_ptr`] to a
    /// `SECURITY_ATTRIBUTES.lpSecurityDescriptor`.
    pub struct SecurityDescriptor {
        psd: PSECURITY_DESCRIPTOR,
    }

    impl SecurityDescriptor {
        /// Parse `sddl` into a self-relative security descriptor.
        pub fn from_sddl(sddl: &str) -> Result<Self, String> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            // SAFETY: `wide` is a valid null-terminated SDDL string that outlives
            // the call; `psd` receives a LocalAlloc'd descriptor we own + free.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(wide.as_ptr()),
                    SDDL_REVISION_1,
                    &mut psd,
                    None,
                )
                .map_err(|e| format!("parse SDDL: {e}"))?;
            }
            Ok(Self { psd })
        }

        /// The raw descriptor pointer for `SECURITY_ATTRIBUTES`.
        pub fn as_ptr(&self) -> *mut core::ffi::c_void {
            self.psd.0
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.psd.0.is_null() {
                // SAFETY: psd came from ConvertStringSecurityDescriptor... which
                // allocates with LocalAlloc; free exactly once here.
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.psd.0)));
                }
                self.psd.0 = std::ptr::null_mut();
            }
        }
    }

    /// The full image path of the process on the CLIENT end of `pipe`.
    pub fn client_image_path(pipe: HANDLE) -> Result<PathBuf, String> {
        let mut pid = 0u32;
        // SAFETY: `pipe` is a live connected pipe handle; `pid` is a stack local.
        unsafe {
            GetNamedPipeClientProcessId(pipe, &mut pid)
                .map_err(|e| format!("GetNamedPipeClientProcessId: {e}"))?;
        }
        process_image_path(pid)
    }

    /// The full image path of the process on the SERVER end of `pipe`.
    pub fn server_image_path(pipe: HANDLE) -> Result<PathBuf, String> {
        let mut pid = 0u32;
        // SAFETY: `pipe` is a live connected pipe handle; `pid` is a stack local.
        unsafe {
            GetNamedPipeServerProcessId(pipe, &mut pid)
                .map_err(|e| format!("GetNamedPipeServerProcessId: {e}"))?;
        }
        process_image_path(pid)
    }

    /// Resolve a PID to its full Win32 image path (needs only
    /// PROCESS_QUERY_LIMITED_INFORMATION, available across integrity levels for
    /// the same user).
    fn process_image_path(pid: u32) -> Result<PathBuf, String> {
        // SAFETY: OpenProcess with a limited-query right; the handle is closed on
        // every path; QueryFullProcessImageNameW writes into a sized buffer.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
            let mut buf = vec![0u16; 32768];
            let mut size = buf.len() as u32;
            let res = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            res.map_err(|e| format!("QueryFullProcessImageName: {e}"))?;
            let s = String::from_utf16_lossy(&buf[..size as usize]);
            Ok(PathBuf::from(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sddl_grants_only_user_and_admins_protected() {
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001");
        assert!(sddl.starts_with("D:P"), "DACL must be protected");
        assert!(sddl.contains("(A;;GA;;;S-1-5-21-1-2-3-1001)"));
        assert!(sddl.contains("(A;;GA;;;BA)"), "admins allowed");
        // No allow-everyone / allow-world ACE.
        assert!(!sddl.contains(";;;WD)"), "must not grant Everyone");
    }

    #[test]
    fn sibling_image_same_dir_matches_other_dir_does_not() {
        let app = PathBuf::from(r"C:\Program Files\Driven\driven.exe");
        let helper = PathBuf::from(r"C:\Program Files\Driven\driven-vss-helper.exe");
        let evil = PathBuf::from(r"C:\Temp\driven-vss-helper.exe");
        assert!(is_sibling_image(&app, &helper));
        assert!(is_sibling_image(&helper, &app));
        assert!(!is_sibling_image(&app, &evil));
    }

    #[test]
    fn sibling_image_is_case_insensitive() {
        // Windows paths are case-insensitive; the helper only handles Windows
        // image paths, so the check is case-insensitive on every test host.
        let a = PathBuf::from(r"C:\Program Files\Driven\driven.exe");
        let b = PathBuf::from(r"c:\program files\driven\driven-vss-helper.exe");
        assert!(is_sibling_image(&a, &b));
    }

    #[test]
    fn image_without_parent_is_not_a_sibling() {
        let a = PathBuf::from("driven.exe");
        let b = PathBuf::from("driven-vss-helper.exe");
        assert!(!is_sibling_image(&a, &b));
    }
}
