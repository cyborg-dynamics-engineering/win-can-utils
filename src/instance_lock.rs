//! Process-lifetime reservations for adapter interfaces and server pipe names.

use std::ffi::c_void;
use std::io;
use std::ptr;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// Reserves a named resource until this value is dropped or the process exits.
/// The mutex is not owned by a thread; the existence of its handle is the lease.
pub struct InstanceLock(*mut c_void);

// Kernel handles can be closed on any thread. No thread owns this mutex.
unsafe impl Send for InstanceLock {}
unsafe impl Sync for InstanceLock {}

impl InstanceLock {
    pub fn acquire(kind: &str, key: &str) -> io::Result<Self> {
        if key.is_empty() || key.contains(['\\', '\0']) || key.encode_utf16().count() > 180 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid resource name",
            ));
        }
        // Pipe names are case insensitive. Canonical USB identifiers are ASCII.
        let name: Vec<_> = format!("Global\\win-can-utils.{kind}.{}", key.to_lowercase())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        if error == 183 {
            // ERROR_ALREADY_EXISTS
            unsafe {
                CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{key} is in use by another server"),
            ));
        }
        Ok(Self(handle))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_exclusive_case_insensitive_and_released() {
        let key = format!("test-{}", std::process::id());
        let first = InstanceLock::acquire("test", &key).unwrap();
        assert!(
            matches!(InstanceLock::acquire("test", &key.to_uppercase()), Err(e)
            if e.kind() == io::ErrorKind::AddrInUse)
        );
        let other = InstanceLock::acquire("test", &format!("{key}-other")).unwrap();
        drop(first);
        assert!(InstanceLock::acquire("test", &key).is_ok());
        drop(other);
    }
}
