//! Single Slint event-loop worker for all airtalk UI windows.
//!
//! Slint's platform state is thread-bound: the first thread to create
//! a Slint window owns the event loop, and no other thread can create
//! Slint windows thereafter. Both Settings and Recovery must therefore
//! share one worker. Requests queue on a channel; the worker runs one
//! window to completion before picking up the next.
//!
//! In practice the two windows don't overlap — Settings is a modal
//! configuration screen the user interacts with, Recovery is a
//! transient notification driven by background paste failures — so
//! queueing is acceptable. If Settings is open and paste fails, the
//! recovery popup appears after the user closes Settings.
//!
//! External `hide_recovery()` uses `slint::invoke_from_event_loop` to
//! reach into the currently-running event loop and fire the popup's
//! dismiss callback, which quits the loop. The same callback path is
//! reused for the × button, Esc, and the Dismiss button so all three
//! dismissal routes converge.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::Result;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use tokio::sync::mpsc as tokio_mpsc;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ScreenToClient, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos,
    GWL_EXSTYLE, HTCAPTION, HTCLIENT, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, WM_NCHITTEST,
    WS_EX_TOOLWINDOW,
};

use crate::paste;
use crate::settings::{self, SettingsEvent};
use crate::slint_ui::RecoveryWindow;

// ─── Public types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryEvent {
    Copied,
    Dismissed,
}

/// Cheap clonable show-only handle so `spawn_blocking` paste workers
/// can trigger a recovery popup without borrowing `SlintBridge`.
#[derive(Clone)]
pub struct RecoveryHandle {
    tx: std::sync::mpsc::Sender<Request>,
}

impl RecoveryHandle {
    pub fn show(&self, text: String) {
        let _ = self.tx.send(Request::ShowRecovery(text));
    }
}

pub struct SlintBridge {
    request_tx: std::sync::mpsc::Sender<Request>,
    settings_is_open: Arc<AtomicBool>,
    _worker: JoinHandle<()>,
}

/// Receiver-side of the bridge. Split from `SlintBridge` so that
/// `tokio::select!` branches can borrow the settings and recovery
/// receivers independently — a single `&mut self` method can only be
/// in one select arm at a time.
pub struct SlintEvents {
    pub settings: tokio_mpsc::UnboundedReceiver<SettingsEvent>,
    pub recovery: tokio_mpsc::UnboundedReceiver<RecoveryEvent>,
}

// ─── Internal state ────────────────────────────────────────────────────

enum Request {
    OpenSettings,
    ShowRecovery(String),
}

/// Weak ref to the currently-visible RecoveryWindow, so a cross-thread
/// `hide_recovery()` can post a dismiss into the Slint event loop.
/// None while recovery isn't showing.
static RECOVERY_WEAK: Mutex<Option<slint::Weak<RecoveryWindow>>> = Mutex::new(None);

// ─── Bridge ────────────────────────────────────────────────────────────

impl SlintBridge {
    pub fn new() -> (Self, SlintEvents) {
        let (settings_event_tx, settings_events) = tokio_mpsc::unbounded_channel();
        let (recovery_event_tx, recovery_events) = tokio_mpsc::unbounded_channel();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let settings_is_open = Arc::new(AtomicBool::new(false));

        let is_open = settings_is_open.clone();
        let worker = std::thread::Builder::new()
            .name("airtalk-slint".into())
            .spawn(move || {
                while let Ok(req) = request_rx.recv() {
                    match req {
                        Request::OpenSettings => {
                            let event = run_settings_iteration();
                            // Release the open flag BEFORE delivering the event — a
                            // main-loop handler that reopens on validation failure
                            // would otherwise be blocked by a stale flag.
                            is_open.store(false, Ordering::Release);
                            if settings_event_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Request::ShowRecovery(text) => {
                            let event = run_recovery_iteration(text);
                            if recovery_event_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .expect("spawn slint thread");

        (
            Self {
                request_tx,
                settings_is_open,
                _worker: worker,
            },
            SlintEvents {
                settings: settings_events,
                recovery: recovery_events,
            },
        )
    }

    pub fn open_settings(&self) {
        if self.settings_is_open.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.request_tx.send(Request::OpenSettings).is_err() {
            self.settings_is_open.store(false, Ordering::Release);
        }
    }

    /// Show the recovery popup directly from the thread that owns the
    /// bridge. Most callers get a clonable [`RecoveryHandle`] instead
    /// (so `spawn_blocking` paste workers don't have to borrow the
    /// bridge) and go through that — but this symmetric API exists for
    /// call sites that already hold `&SlintBridge`.
    #[allow(dead_code)]
    pub fn show_recovery(&self, text: String) {
        let _ = self.request_tx.send(Request::ShowRecovery(text));
    }

    /// Dismiss the currently-visible recovery popup. Safe to call from
    /// any thread; no-op if recovery isn't showing. We specifically
    /// guard on RECOVERY_WEAK to avoid quitting a Settings event loop
    /// that happens to be running when a stale hide fires.
    pub fn hide_recovery(&self) {
        let weak = RECOVERY_WEAK.lock().unwrap().clone();
        if weak.is_none() {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.and_then(|w| w.upgrade()) {
                w.invoke_dismiss_requested();
            }
        });
    }

    pub fn recovery_handle(&self) -> RecoveryHandle {
        RecoveryHandle {
            tx: self.request_tx.clone(),
        }
    }
}

// ─── Window iterations ─────────────────────────────────────────────────

fn run_settings_iteration() -> SettingsEvent {
    match settings::run_settings_window() {
        Ok(Some(req)) => match settings::save_request(req).and_then(|_| settings::load_snapshot()) {
            Ok(snapshot) => SettingsEvent::Applied(snapshot),
            Err(e) => SettingsEvent::Failed(format!("{e:#}")),
        },
        Ok(None) => SettingsEvent::Cancelled,
        Err(e) => SettingsEvent::Failed(format!("{e:#}")),
    }
}

fn run_recovery_iteration(text: String) -> RecoveryEvent {
    let window = match RecoveryWindow::new() {
        Ok(w) => w,
        Err(e) => {
            log::error!("RecoveryWindow::new failed: {e}");
            return RecoveryEvent::Dismissed;
        }
    };
    window.set_body_text(text.clone().into());

    let result: Arc<Mutex<RecoveryEvent>> = Arc::new(Mutex::new(RecoveryEvent::Dismissed));

    {
        let result = result.clone();
        let text = text.clone();
        window.on_copy_requested(move || {
            match paste::copy_to_clipboard(&text) {
                Ok(()) => {
                    log::info!(
                        "recovery: copied {} chars to clipboard",
                        text.chars().count()
                    );
                    *result.lock().unwrap() = RecoveryEvent::Copied;
                }
                Err(e) => {
                    log::warn!("recovery clipboard copy failed: {e:#}");
                    *result.lock().unwrap() = RecoveryEvent::Dismissed;
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    {
        let result = result.clone();
        window.on_dismiss_requested(move || {
            *result.lock().unwrap() = RecoveryEvent::Dismissed;
            let _ = slint::quit_event_loop();
        });
    }

    *RECOVERY_WEAK.lock().unwrap() = Some(window.as_weak());

    // Capture the window that currently has user focus BEFORE show()
    // activates our popup — we want to (a) anchor the popup on that
    // window's monitor and (b) hand focus back to it after positioning
    // so the user's typing context (e.g. the PowerShell they're in)
    // isn't stolen.
    let prev_fg = SendHwnd(unsafe { GetForegroundWindow() });

    if let Err(e) = window.show() {
        log::error!("RecoveryWindow::show failed: {e}");
        *RECOVERY_WEAK.lock().unwrap() = None;
        return RecoveryEvent::Dismissed;
    }

    // Defer fixups to the next event-loop tick: running them
    // synchronously right after `show()` races with winit's platform
    // window setup and can land the popup at the OS default position
    // (top-left). By the time this closure fires, the window is fully
    // created and our SetWindowPos sticks.
    {
        let weak = window.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if let Err(e) = apply_recovery_win32_fixups(&w, prev_fg) {
                    log::warn!("recovery Win32 fixups failed: {e:#} (popup still works)");
                }
            }
        });
    }

    if let Err(e) = slint::run_event_loop() {
        log::warn!("recovery event loop ended with error: {e}");
    }

    *RECOVERY_WEAK.lock().unwrap() = None;
    let _ = window.hide();

    let out = result.lock().unwrap().clone();
    out
}

// ─── Win32 fixups Slint doesn't expose ────────────────────────────────

/// Send wrapper for HWND. Needed because we pass the previous
/// foreground window through `invoke_from_event_loop` (→ different
/// closure capture, HWND's inner `*mut c_void` isn't Send).
#[derive(Copy, Clone)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

/// Logical-pixel height of the drag-by-title region at 96 DPI — the
/// subclass scales this by the current window DPI at hit-test time.
/// Roughly matches: 16 px top padding + ~32 px for title + hint +
/// a few pixels slack.
const HEADER_DRAG_LOGICAL_H: i32 = 56;
/// Logical width of the close-glyph hot-zone on the right side of
/// the header. Must stay larger than `CloseGlyph`'s visible area so
/// clicks on × dismiss instead of starting a drag.
const HEADER_CLOSE_LOGICAL_W: i32 = 48;

/// Apply the Win32-level behavior Slint's `Window` can't express:
///
///   * `WS_EX_TOOLWINDOW`                   hide from Alt+Tab
///   * window subclass                      native drag via HTCAPTION
///                                          in the header region
///   * DWM rounded corners                  Win 11 only (square on Win 10)
///   * `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)` — anchor to
///                                          `prev_fg`'s monitor
///
/// We intentionally **let the popup take focus on show**. Experiments
/// with `SetForegroundWindow(prev_fg)` + `MA_NOACTIVATE` + `forward-focus`
/// all tripped over Slint/winit's input dispatch: without OS focus the
/// first click on any button was silently absorbed as an activation
/// update and only the second click fired. A brief focus flicker in the
/// previously-foreground app is the right tradeoff for a rare-event
/// "paste failed" popup — it demands attention anyway.
fn apply_recovery_win32_fixups(window: &RecoveryWindow, prev_fg: SendHwnd) -> Result<()> {
    let hwnd = raw_hwnd(window)?;
    let prev_fg = prev_fg.0;
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex = (current as u32) | WS_EX_TOOLWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex as isize);

        // Install the subclass (handles WM_NCHITTEST for drag +
        // WM_MOUSEACTIVATE for click-through).
        let _ = SetWindowSubclass(hwnd, Some(recovery_subclass_proc), 0, 0);

        // DWM rounded corners on Win 11; a no-op on Win 10 (square).
        let pref: i32 = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as _,
            std::mem::size_of::<i32>() as u32,
        );

        // Anchor to prev_fg's monitor. Using the popup's own HWND
        // here would return the foreground (which is the popup itself
        // after show) and stick us on the primary monitor every time.
        let mon_rect = monitor_work_area_for(prev_fg);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let w = client.right - client.left;
        let h = client.bottom - client.top;
        let mon_w = mon_rect.right - mon_rect.left;
        let x = mon_rect.left + (mon_w - w) / 2;
        let y = mon_rect.bottom - h - 140;

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    Ok(())
}

/// Subclass procedure for the recovery popup. Only job: return
/// `HTCAPTION` from `WM_NCHITTEST` inside the header drag region so
/// Windows drives the modal drag loop natively (Slint's TouchArea
/// never sees the pointer-down, so no stuck-cursor state on release).
/// `HTCLIENT` everywhere else so buttons, scroll, and the × glyph
/// get normal client hit-testing.
///
/// We deliberately do NOT intercept `WM_MOUSEACTIVATE`. Overriding it
/// with `MA_NOACTIVATE` sounds right — we just restored the previous
/// foreground so the popup is un-focused — but it confuses winit's
/// input dispatch: the first click on any button silently updates
/// activation state without firing the TouchArea, and only the second
/// click triggers the handler. Letting the default `MA_ACTIVATE`
/// through means clicking the popup briefly focuses it (expected
/// when you click a button) and the click fires on attempt one.
unsafe extern "system" fn recovery_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    if msg == WM_NCHITTEST {
        // lparam packs screen coords: low i16 = x, high i16 = y.
        let raw = lparam.0 as u32;
        let sx = (raw & 0xFFFF) as i16 as i32;
        let sy = ((raw >> 16) & 0xFFFF) as i16 as i32;
        let mut pt = POINT { x: sx, y: sy };
        if ScreenToClient(hwnd, &mut pt).as_bool() {
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_ok() {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let drag_h = HEADER_DRAG_LOGICAL_H * dpi as i32 / 96;
                let close_w = HEADER_CLOSE_LOGICAL_W * dpi as i32 / 96;
                let in_header_y = pt.y >= 0 && pt.y < drag_h;
                let inside_close = pt.x > rect.right - close_w;
                if in_header_y && !inside_close {
                    return LRESULT(HTCAPTION as isize);
                }
            }
        }
        return LRESULT(HTCLIENT as isize);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn raw_hwnd(window: &RecoveryWindow) -> Result<HWND> {
    let slint_window = window.window();
    // Two-step binding: `window_handle()` returns a by-value
    // `slint::WindowHandle`, and the raw-window-handle v0.6
    // `WindowHandle<'_>` borrows from it. Without the binding the
    // intermediate drops at end-of-statement.
    let slint_handle = slint_window.window_handle();
    let handle = slint_handle
        .window_handle()
        .map_err(|e| anyhow::anyhow!("window_handle(): {e:?}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut c_void)),
        other => anyhow::bail!("unexpected window handle variant: {other:?}"),
    }
}

/// Work area of the monitor hosting `anchor_hwnd`. Falls back to the
/// primary monitor when `anchor_hwnd` is null or unreachable.
unsafe fn monitor_work_area_for(anchor_hwnd: HWND) -> RECT {
    let monitor = if anchor_hwnd.0.is_null() {
        MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
    } else {
        MonitorFromWindow(anchor_hwnd, MONITOR_DEFAULTTONEAREST)
    };
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    }
}
