//! Full redraws the moment Windows may have dropped a window's content. The software renderer
//! draws only what changed, so after a move to another monitor, a DPI or display change, or the
//! window coming back to the front, parts stayed see-through or showed an older page. The
//! window's messages are watched (a subclass) and each such event asks for one full redraw.
use std::cell::RefCell;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{WM_ACTIVATE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_EXITSIZEMOVE, WM_SHOWWINDOW, WM_WINDOWPOSCHANGED};

thread_local! {
    /// Marks the whole window dirty (bumps its `repaint` property). UI thread only.
    static BUMP: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
}

/// Watches `hwnd` (one window per process) and calls `bump` on the UI thread after each event.
pub fn watch(hwnd: HWND, bump: impl Fn() + 'static) {
    BUMP.with_borrow_mut(|b| *b = Some(Box::new(bump)));
    unsafe { let _ = SetWindowSubclass(hwnd, Some(on_message), 1, 0); }
}

unsafe extern "system" fn on_message(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if matches!(msg, WM_WINDOWPOSCHANGED | WM_ACTIVATE | WM_SHOWWINDOW | WM_DPICHANGED | WM_DISPLAYCHANGE | WM_EXITSIZEMOVE) {
        // After the message is handled (the new position / size is in place), not inside it.
        let _ = slint::invoke_from_event_loop(|| BUMP.with_borrow(|b| if let Some(f) = b { f() }));
    }
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}
