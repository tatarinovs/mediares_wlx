//! Owned mailboxes: window messages carry only a session token, never an owning pointer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_MEDIA_EVENT: u32 = WM_APP + 0x10;

pub fn next_token() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy)]
pub struct Event {
    pub kind: i32,
    pub parameter: isize,
}

struct State {
    token: usize,
    active: bool,
    pending: Vec<Event>,
}

#[derive(Clone)]
pub struct Events {
    target: isize,
    state: Arc<Mutex<State>>,
}

impl Events {
    pub fn new(target: HWND) -> Self {
        Self {
            target: target.0 as isize,
            state: Arc::new(Mutex::new(State {
                token: next_token(),
                active: true,
                pending: Vec::new(),
            })),
        }
    }
    pub fn token(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).token
    }
    pub fn begin_file(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.token = next_token();
        state.pending.clear();
    }
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = false;
        state.pending.clear();
    }
    pub fn post(&self, token: usize, kind: i32, parameter: isize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active || state.token != token {
            return;
        }
        // Notifications describe the current player state, so one pending event per kind suffices.
        let notify = state.pending.is_empty();
        state.pending.retain(|event| event.kind != kind);
        state.pending.push(Event { kind, parameter });
        if notify {
            let posted = unsafe {
                PostMessageW(
                    Some(HWND(self.target as *mut _)),
                    WM_MEDIA_EVENT,
                    WPARAM(token),
                    LPARAM(0),
                )
            };
            if posted.is_err() {
                state.pending.clear();
            }
        }
    }
    pub fn take(&self, token: usize) -> Vec<Event> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.token != token || !state.active {
            return Vec::new();
        }
        std::mem::take(&mut state.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_sessions_cannot_deliver_events_into_a_new_file_or_window() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
        };
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WINDOW_STYLE(0),
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        let events = Events::new(window);
        let old = events.token();
        events.post(old, 13, 7);
        events.begin_file();
        events.post(old, 13, 0);
        assert!(events.take(old).is_empty());
        assert!(events.take(events.token()).is_empty());
        events.post(events.token(), 10, 42);
        assert!(events.take(old).is_empty());
        let current = events.take(events.token());
        assert_eq!(
            (current.len(), current[0].kind, current[0].parameter),
            (1, 10, 42)
        );
        let newer = Events::new(window);
        assert_ne!(events.token(), newer.token());
        events.close();
        events.post(events.token(), 13, 0);
        assert!(events.take(events.token()).is_empty());
        unsafe {
            DestroyWindow(window).unwrap();
        }
    }
}
