//! One instance per session (tray, observer): a named mutex in the session's `Local\` namespace.
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

/// Held for as long as the instance runs; dropping it lets the next one start.
pub struct Instance(HANDLE);

impl Drop for Instance {
    fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } }
}

/// `Some` if this is the only holder of `Local\<name>` in the session, `None` if another runs.
pub fn acquire(name: &str) -> Option<Instance> {
    let h = unsafe { CreateMutexW(None, true, &HSTRING::from(format!("Local\\{name}"))) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { let _ = CloseHandle(h); }
        return None;
    }
    Some(Instance(h))
}

/// Whether some process holds `Local\<name>` now (without taking it).
pub fn running(name: &str) -> bool {
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(format!("Local\\{name}"))) } {
        Ok(h) => { unsafe { let _ = CloseHandle(h); } true }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn second_acquire_fails_until_dropped() {
        let name = format!("keel-test-{}", std::process::id());
        let a = super::acquire(&name).expect("first");
        assert!(super::acquire(&name).is_none());
        assert!(super::running(&name));
        drop(a);
        assert!(!super::running(&name));
        assert!(super::acquire(&name).is_some());
    }
}
