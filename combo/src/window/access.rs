//! Serialize access to a viewer across synchronous Win32 callbacks on its UI thread.
//! Nested messages never borrow ViewerState; notifications resume after the outer operation.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
struct Active {
    bounds: Option<RECT>,
    pending: Vec<(u32, WPARAM, LPARAM)>,
    paint: Vec<HWND>,
    retired: Option<*mut ViewerState>,
}

thread_local! {
    static ACTIVE: RefCell<HashMap<isize, Active>> = RefCell::new(HashMap::new());
}

pub(super) unsafe fn run<R>(hwnd: HWND, fallback: R, work: impl FnOnce() -> R) -> R {
    let key = hwnd.0 as isize;
    let entered = ACTIVE.with_borrow_mut(|active| {
        if active.contains_key(&key) {
            return false;
        }
        let bounds = get_state(hwnd).and_then(|s| s.fullscreen);
        active.insert(
            key,
            Active {
                bounds,
                ..Default::default()
            },
        );
        true
    });
    if !entered {
        return fallback;
    }
    struct Exit(HWND);
    impl Drop for Exit {
        fn drop(&mut self) {
            let Some(active) = ACTIVE.with_borrow_mut(|a| a.remove(&(self.0 .0 as isize))) else {
                return;
            };
            unsafe {
                if let Some(ptr) = active.retired {
                    // Create the owning Box only after the outer borrow has ended.
                    drop(Box::from_raw(ptr));
                    crate::transport_bar::save_audio_level();
                    crate::smooth::release();
                    gdi::release_buffers();
                    return;
                }
                for (msg, wp, lp) in active.pending {
                    let _ = PostMessageW(Some(self.0), msg, wp, lp);
                }
                for window in active.paint {
                    let _ = InvalidateRect(Some(window), None, false);
                    let _ = UpdateWindow(window);
                }
            }
        }
    }
    let _exit = Exit(hwnd);
    work()
}

pub(crate) fn fullscreen(hwnd: HWND, bounds: Option<RECT>) {
    ACTIVE.with_borrow_mut(|a| {
        if let Some(active) = a.get_mut(&(hwnd.0 as isize)) {
            active.bounds = bounds;
        }
    });
}

pub(super) fn defer_close(hwnd: HWND) -> bool {
    ACTIVE.with_borrow_mut(|a| {
        let Some(active) = a.get_mut(&(hwnd.0 as isize)) else {
            return false;
        };
        if !active.pending.iter().any(|&(msg, _, _)| msg == WM_CLOSE) {
            active.pending.push((WM_CLOSE, WPARAM(0), LPARAM(0)));
        }
        true
    })
}

/// Handle nested messages without creating references into the viewer being mutated.
pub(super) unsafe fn nested(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let result = ACTIVE.with_borrow_mut(|a| {
        let active = a.get_mut(&(hwnd.0 as isize))?;
        match msg {
            WM_NCDESTROY => {
                let ptr = crate::set_window_long_ptr(hwnd, GWLP_USERDATA, 0) as *mut ViewerState;
                if !ptr.is_null() {
                    active.retired = Some(ptr);
                }
            }
            WM_PAINT => {
                if !active.paint.contains(&hwnd) {
                    active.paint.push(hwnd);
                }
            }
            WM_WINDOWPOSCHANGING => {
                if let Some(pos) = (lp.0 as *mut WINDOWPOS).as_mut() {
                    fullscreen::pin_bounds(active.bounds, pos);
                }
            }
            WM_CLOSE | WM_SIZE | WM_MEDIA_EVENT | WM_IMAGE_READY | WM_DIR_SCANNED
            | WM_CONTACT_SHEET | WM_SEEK_PREVIEW | WM_OVERLAY_COMMAND
                if !active
                    .pending
                    .iter()
                    .any(|&(m, w, l)| m == msg && w == wp && l == lp) =>
            {
                // These messages own no pointers. Preserve distinct session tokens.
                active.pending.push((msg, wp, lp));
            }
            _ => {}
        }
        Some(LRESULT(0))
    });
    if result.is_some() && msg == WM_PAINT {
        // BeginPaint can itself send WM_ERASEBKGND: no RefCell borrow may span it.
        let mut ps = PAINTSTRUCT::default();
        BeginPaint(hwnd, &mut ps);
        let _ = EndPaint(hwnd, &ps);
    }
    result
}

pub(super) fn defer_overlay(viewer: HWND, window: HWND, msg: u32) -> bool {
    ACTIVE.with_borrow_mut(|a| {
        let Some(active) = a.get_mut(&(viewer.0 as isize)) else {
            return false;
        };
        if msg == WM_PAINT && !active.paint.contains(&window) {
            active.paint.push(window);
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_notifications_wait_and_fullscreen_geometry_needs_no_viewer_borrow() {
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                Default::default(),
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            run(hwnd, (), || {
                assert_eq!(run(hwnd, 7, || panic!("nested access")), 7);
                fullscreen(
                    hwnd,
                    Some(RECT {
                        left: -100,
                        top: 10,
                        right: 900,
                        bottom: 710,
                    }),
                );
                let mut pos = WINDOWPOS::default();
                nested(
                    hwnd,
                    WM_WINDOWPOSCHANGING,
                    WPARAM(0),
                    LPARAM((&mut pos as *mut WINDOWPOS) as isize),
                );
                assert_eq!((pos.x, pos.y, pos.cx, pos.cy), (-100, 10, 1000, 700));
                nested(hwnd, WM_MEDIA_EVENT, WPARAM(31), LPARAM(0));
                nested(hwnd, WM_MEDIA_EVENT, WPARAM(32), LPARAM(0));
                assert!(defer_close(hwnd));
                let mut message = MSG::default();
                assert!(!PeekMessageW(
                    &mut message,
                    Some(hwnd),
                    WM_MEDIA_EVENT,
                    WM_MEDIA_EVENT,
                    PM_REMOVE
                )
                .as_bool());
            });
            let mut message = MSG::default();
            for token in [31, 32] {
                assert!(PeekMessageW(
                    &mut message,
                    Some(hwnd),
                    WM_MEDIA_EVENT,
                    WM_MEDIA_EVENT,
                    PM_REMOVE
                )
                .as_bool());
                assert_eq!(message.wParam.0, token);
            }
            assert!(
                PeekMessageW(&mut message, Some(hwnd), WM_CLOSE, WM_CLOSE, PM_REMOVE).as_bool()
            );
            DestroyWindow(hwnd).unwrap();
        }
    }
}
