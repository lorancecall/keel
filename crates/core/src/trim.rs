//! Gives the working set back to Windows after a burst of work (start-up, a scan, a sample):
//! resident processes then sit at a few MB instead of their peak. RAM is this machine's bottleneck.
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

pub fn working_set() {
    unsafe { let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX); }
}
