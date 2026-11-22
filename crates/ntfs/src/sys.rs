//! Win32 FFI. The only module of the crate allowed to use `unsafe`; everything it exposes is a
//! safe wrapper. Output parsing happens elsewhere, in safe code, on the returned byte slices.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// An owned Win32 handle, closed on drop.
#[derive(Debug)]
pub(crate) struct Handle(HANDLE);

// SAFETY: a kernel handle is just an index into the process handle table; it may be used and
// closed from any thread. We never hand out the raw value, and every use goes through `&self`
// calls that the kernel serializes per handle.
unsafe impl Send for Handle {}
// SAFETY: see above; DeviceIoControl/ReadFile with explicit offsets are thread-safe on a
// synchronous handle (no shared file pointer is used).
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle owned exclusively by this value (constructed only
        // from a successful CreateFileW/OpenProcessToken) and is closed exactly once here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens a device path such as `\\.\C:` with the given access mask, sharing everything.
pub(crate) fn open_device(path: &str, access: u32) -> io::Result<Handle> {
    let path = wide(path);
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call; the security
    // attributes and template handle are documented as optional (null).
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(h))
    }
}

/// A zeroed, 4 KiB-aligned byte buffer (sector-aligned for raw volume reads, 8-byte aligned
/// as `USN_RECORD`s require).
pub(crate) struct AlignedBuf {
    pages: Vec<Page>,
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

impl AlignedBuf {
    /// A buffer of at least `len` bytes (rounded up to 4 KiB).
    pub(crate) fn new(len: usize) -> Self {
        Self {
            pages: vec![Page([0; 4096]); len.div_ceil(4096).max(1)],
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len() * 4096
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: `Page` is `repr(C)` around `[u8; 4096]` with no padding (size == 4096 ==
        // align), so the `pages` allocation is `len()` contiguous initialized bytes; `u8` has
        // alignment 1, and the borrow of `self` keeps the allocation alive and unaliased-mut.
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr().cast::<u8>(), self.len()) }
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.len();
        // SAFETY: as in `bytes`, and the `&mut self` borrow guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr().cast::<u8>(), len) }
    }
}

/// `DeviceIoControl` with an optional typed input struct and a byte output buffer.
/// Returns the number of bytes written to `out`.
pub(crate) fn ioctl<T: Copy>(
    h: &Handle,
    code: u32,
    input: Option<&T>,
    out: &mut [u8],
) -> io::Result<usize> {
    let (in_ptr, in_len) = match input {
        Some(t) => ((t as *const T).cast::<c_void>(), size_of::<T>() as u32),
        None => (std::ptr::null(), 0),
    };
    let out_len = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let mut returned = 0u32;
    // SAFETY: `h` is a live handle; `in_ptr`/`in_len` describe a valid `T` (plain-old-data
    // Win32 struct) or are null/0; `out` is a valid writable buffer of at least `out_len`
    // bytes for the duration of this synchronous call (no OVERLAPPED, so the kernel does not
    // keep the pointers after returning); `returned` is a valid out-pointer.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            in_ptr,
            in_len,
            out.as_mut_ptr().cast::<c_void>(),
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((returned as usize).min(out.len()))
    }
}

/// Reads up to `buf.len()` bytes at absolute byte `offset` (synchronous positioned read).
pub(crate) fn read_at(h: &Handle, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut ov = OVERLAPPED::default();
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let mut read = 0u32;
    // SAFETY: `h` is a live synchronous handle, so ReadFile completes before returning and the
    // kernel does not retain `buf` or `ov`; `buf` is valid for `len` writable bytes; `ov`
    // only carries the start offset; `read` is a valid out-pointer.
    let ok = unsafe { ReadFile(h.0, buf.as_mut_ptr(), len, &mut read, &mut ov) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((read as usize).min(buf.len()))
    }
}

/// `GetDriveTypeW` for a root such as `C:\`.
pub(crate) fn drive_type(root: &str) -> u32 {
    let root = wide(root);
    // SAFETY: `root` is a NUL-terminated UTF-16 string valid for the call.
    unsafe { GetDriveTypeW(root.as_ptr()) }
}

/// `GetLogicalDrives`: bit `n` set = drive letter `A + n` exists. One cheap call, no disk I/O.
pub(crate) fn logical_drives() -> u32 {
    // SAFETY: takes no arguments and only reads process-independent state.
    unsafe { GetLogicalDrives() }
}

/// Volume serial number of a root such as `E:\` (`GetVolumeInformationW`). Fails fast with
/// "device not ready" for an empty card-reader slot.
pub(crate) fn volume_serial(root: &str) -> io::Result<u32> {
    let root = wide(root);
    let mut serial = 0u32;
    // SAFETY: `root` is NUL-terminated; `serial` is a valid out-pointer; the other buffers and
    // out-parameters are optional and passed as null with size 0.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(serial)
    }
}

/// File-system name (`NTFS`, `FAT32`, `exFAT`, `ReFS`, ...) of a root such as `C:\`.
pub(crate) fn filesystem_name(root: &str) -> io::Result<String> {
    let root = wide(root);
    let mut fs = [0u16; 64];
    // SAFETY: `root` is NUL-terminated; `fs` is a writable buffer whose length (in u16) is
    // passed; all other out-parameters are optional and passed as null with size 0.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    Ok(String::from_utf16_lossy(&fs[..end]))
}

/// True if the current process token is elevated (`TokenElevation`).
pub(crate) fn is_elevated() -> io::Result<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; `token` is a
    // valid out-pointer.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `token.0` is a live token handle opened with TOKEN_QUERY; the output buffer is a
    // `TOKEN_ELEVATION` whose exact size is passed; `len` is a valid out-pointer.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buf() {
        let mut b = AlignedBuf::new(5000);
        assert_eq!(b.len(), 8192);
        assert_eq!(b.bytes().as_ptr() as usize % 4096, 0);
        b.bytes_mut()[8191] = 7;
        assert_eq!(b.bytes()[8191], 7);
    }

    #[test]
    fn elevation_query_works() {
        // Either answer is fine; the call itself must succeed without privileges.
        is_elevated().unwrap();
    }

    #[test]
    fn system_drive_is_queryable_without_privileges() {
        let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\";
        assert_eq!(drive_type(&root), 3, "DRIVE_FIXED");
        assert!(!filesystem_name(&root).unwrap().is_empty());
    }
}
