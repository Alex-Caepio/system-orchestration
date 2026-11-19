use crate::Result;
use std::path::Path;

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub struct DrainGuard(std::fs::File);

impl DrainGuard {
    pub fn acquire(folder: &Path, name: &str) -> Result<Option<Self>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        let file = match options.open(folder.join(format!(".{name}.drain-lock"))) {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        if !file.metadata()?.is_file() {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Ok(None);
            }
        }
        #[cfg(windows)]
        {
            if !windows_lock::lock(&file, false) {
                return Ok(None);
            }
        }
        Ok(Some(Self(file)))
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(windows)]
        {
            let _ = windows_lock::lock(&self.0, true);
        }
    }
}

#[cfg(windows)]
mod windows_lock {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut core::ffi::c_void,
    }

    unsafe extern "system" {
        fn LockFileEx(
            file: *mut core::ffi::c_void,
            flags: u32,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: *mut core::ffi::c_void,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    pub fn lock(file: &std::fs::File, unlock: bool) -> bool {
        let mut overlapped = Overlapped {
            internal: 0,
            high: 0,
            offset: 0,
            offset_high: 0,
            event: core::ptr::null_mut(),
        };
        let handle = file.as_raw_handle();
        let ok = if unlock {
            unsafe { UnlockFileEx(handle, 0, u32::MAX, u32::MAX, &mut overlapped) }
        } else {
            unsafe { LockFileEx(handle, 2 | 1, 0, u32::MAX, u32::MAX, &mut overlapped) }
        };
        ok != 0
    }
}
