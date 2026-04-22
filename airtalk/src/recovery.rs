//! "Paste failed — here's your text" recovery popup.
//!
//! When `paste` returns `Err`, the transcription is worth showing the
//! user with an easy "copy to clipboard" escape hatch — flashing an icon
//! isn't enough, because the text only exists in memory until the next
//! session overwrites it.
//!
//! Visual: a dark rounded panel, topmost but non-activating (doesn't
//! steal focus). Anchored just above the floating pill on whichever
//! monitor hosts the active window. Title line, then the transcript
//! itself, wrapped and clipped to at most 6 visible lines (mouse-wheel
//! scrolls when longer). Single "复制" button at the bottom-right, plus
//! an × close glyph in the top-right corner.
//!
//! Three dismissal paths, all routed through `Recovery::hide()` or the
//! equivalent wndproc handling:
//!   1. User clicks 复制 → text → clipboard, window closes, emits Copied.
//!   2. User clicks × → window closes, emits Dismissed.
//!   3. Main loop calls `hide()` when a new hotkey session starts.
//!
//! Rendering uses GDI (`WM_PAINT` + `DrawTextW`). tiny-skia — the rest
//! of the app's 2-D stack — has no text rasterizer, and pulling in a
//! pure-Rust one (cosmic-text / ab_glyph + fontdb) would cost ~1 MB of
//! binary for one popup. `DrawTextW` with `DT_WORDBREAK` handles CJK +
//! Latin mixed text fine on anything from Win 7 onwards.
//!
//! Rounded corners: `DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE)`
//! on Win 11 gives crisp DWM-rendered rounded corners; Win 10 sees a
//! square window (acceptable — this is a notification, not a billboard).

#![cfg(windows)]

use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::mpsc as tokio_mpsc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DT_CALCRECT, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FW_NORMAL, FW_SEMIBOLD, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetForegroundWindow,
    GetMessageW, LoadCursorW, PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, IDC_ARROW, MSG, SWP_NOACTIVATE,
    SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::paste;

// ─── Layout constants (logical px, scaled per-monitor DPI) ─────────────

const LOGICAL_WIDTH: i32 = 420;
const PADDING: i32 = 16;
const TITLE_HEIGHT: i32 = 24;
const BODY_LINE_HEIGHT: i32 = 22;
const MAX_BODY_LINES: i32 = 6;
const BUTTON_HEIGHT: i32 = 32;
const BUTTON_WIDTH: i32 = 72;
const CLOSE_SIZE: i32 = 22; // the × hit box
                            // Rounded corners come from DwmSetWindowAttribute on Win 11; no manual
                            // region math needed, so CORNER_RADIUS isn't currently referenced. Kept
                            // here as a knob in case we ever add a Win 10 fallback via SetWindowRgn.
#[allow(dead_code)]
const CORNER_RADIUS: i32 = 12;
const BOTTOM_MARGIN: i32 = 116; // clearance above the pill

// Colors (ARGB hex → COLORREF's BGR order below via helper).
// Matches overlay aesthetic: near-black pill with white text.
const BG_COLOR: (u8, u8, u8) = (28, 25, 25); // #1c1919
const FG_COLOR: (u8, u8, u8) = (240, 240, 240);
const FG_SUBTLE: (u8, u8, u8) = (180, 180, 185);
const ACCENT_COLOR: (u8, u8, u8) = (255, 82, 82);
const BUTTON_BG: (u8, u8, u8) = (56, 60, 70);
const BUTTON_BG_HOT: (u8, u8, u8) = (80, 86, 100);

fn colorref(rgb: (u8, u8, u8)) -> COLORREF {
    // Win32 COLORREF is 0x00BBGGRR.
    COLORREF((rgb.2 as u32) << 16 | (rgb.1 as u32) << 8 | (rgb.0 as u32))
}

// ─── Public API ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryEvent {
    Copied,
    Dismissed,
}

pub struct Recovery {
    cmd_tx: mpsc::Sender<Cmd>,
    events: tokio_mpsc::UnboundedReceiver<RecoveryEvent>,
    _thread: JoinHandle<()>,
}

/// Cheap clonable send-side handle so `spawn_blocking` tasks (e.g. the
/// paste worker) can trigger `show` without borrowing the `Recovery`.
#[derive(Clone)]
pub struct RecoveryHandle {
    cmd_tx: mpsc::Sender<Cmd>,
}

#[allow(dead_code)] // hide() only called from the handle route when needed
impl RecoveryHandle {
    pub fn show(&self, text: String) {
        if self.cmd_tx.send(Cmd::Show { text }).is_err() {
            return;
        }
        wake_window();
    }
    pub fn hide(&self) {
        if self.cmd_tx.send(Cmd::Hide).is_err() {
            return;
        }
        wake_window();
    }
}

enum Cmd {
    Show { text: String },
    Hide,
}

impl Recovery {
    pub fn start() -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = tokio_mpsc::unbounded_channel();
        EVENT_TX
            .set(ev_tx)
            .map_err(|_| anyhow!("Recovery::start already called in this process"))?;
        CMD_RX
            .set(Mutex::new(Some(cmd_rx)))
            .map_err(|_| anyhow!("Recovery cmd channel already taken"))?;

        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("airtalk-recovery".into())
            .spawn(move || {
                if let Err(e) = run_thread(&init_tx) {
                    let _ = init_tx.send(Err(e));
                }
            })
            .context("spawn recovery thread")?;

        init_rx
            .recv_timeout(Duration::from_secs(2))
            .context("recovery init timeout (2 s)")?
            .context("recovery init error")?;

        Ok(Self {
            cmd_tx,
            events: ev_rx,
            _thread: thread,
        })
    }

    /// Show the popup with the given transcript text. No-op if already visible;
    /// the previous text is replaced in that case. Most callers use
    /// `handle()` + `RecoveryHandle::show` instead of this method — it exists
    /// for the main thread that owns the `Recovery` directly.
    #[allow(dead_code)]
    pub fn show(&self, text: String) {
        if self.cmd_tx.send(Cmd::Show { text }).is_err() {
            return;
        }
        wake_window();
    }

    /// Hide the popup. Idempotent.
    pub fn hide(&self) {
        if self.cmd_tx.send(Cmd::Hide).is_err() {
            return;
        }
        wake_window();
    }

    pub async fn recv(&mut self) -> Option<RecoveryEvent> {
        self.events.recv().await
    }

    pub fn handle(&self) -> RecoveryHandle {
        RecoveryHandle {
            cmd_tx: self.cmd_tx.clone(),
        }
    }
}

// ─── Shared static state ───────────────────────────────────────────────

static EVENT_TX: OnceLock<tokio_mpsc::UnboundedSender<RecoveryEvent>> = OnceLock::new();
/// Receiver lives on the window thread. Wrapped in Option so it can be
/// `.take()`n out of the Mutex on first access.
static CMD_RX: OnceLock<Mutex<Option<mpsc::Receiver<Cmd>>>> = OnceLock::new();
/// HWND stored for `wake_window()` (PostMessage from UI thread).
static WINDOW_HWND: OnceLock<HwndBox> = OnceLock::new();

struct HwndBox(HWND);
unsafe impl Send for HwndBox {}
unsafe impl Sync for HwndBox {}

/// Custom message used to wake the window thread when a Cmd is waiting.
/// GetMessageW blocks the thread; we PostMessageW this to unblock it.
const WM_APP_WAKE: u32 = WM_APP + 1;

fn wake_window() {
    if let Some(HwndBox(hwnd)) = WINDOW_HWND.get() {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(*hwnd),
                WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

// ─── Thread entry ──────────────────────────────────────────────────────

fn run_thread(init_tx: &mpsc::Sender<Result<()>>) -> Result<()> {
    unsafe {
        let h_instance = GetModuleHandleW(None).context("GetModuleHandleW")?;
        register_window_class(h_instance.into())?;
        let hwnd = create_hidden_window(h_instance.into())?;
        let _ = WINDOW_HWND.set(HwndBox(hwnd));

        // Round the window's corners on Win 11; no-op on Win 10 (square).
        let pref: i32 = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as _,
            std::mem::size_of::<i32>() as u32,
        );

        // Initial font set at the initial monitor's DPI.
        let scale = window_scale(hwnd);
        let mut fonts = Fonts::create(scale);

        let _ = init_tx.send(Ok(()));

        let mut state = State::default();
        state.hwnd = hwnd;
        state.scale = scale;
        state_set(state);

        let cmd_rx = CMD_RX
            .get()
            .and_then(|m| m.lock().ok().and_then(|mut g| g.take()))
            .ok_or_else(|| anyhow!("cmd_rx unavailable"))?;

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 <= 0 {
                break;
            }

            if msg.message == WM_APP_WAKE {
                // Drain all pending commands.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        Cmd::Show { text } => present(hwnd, &fonts, text),
                        Cmd::Hide => dismiss(hwnd, false),
                    }
                }
                continue;
            }

            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        fonts.destroy();
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
    }
    Ok(())
}

// ─── Per-window drawing state ──────────────────────────────────────────

#[derive(Default)]
struct State {
    hwnd: HWND,
    scale: f32,
    visible: bool,
    text: String,
    /// Computed on `present`. Physical px.
    text_total_h: i32,
    /// Pixel offset into the body. 0 = top. Clamped to [0, max_scroll].
    scroll_offset: i32,
    /// Cached button rects (client coords, physical px).
    btn_copy: RECT,
    btn_close: RECT,
    body_rect: RECT,
    hot_copy: bool,
}

/// Single-threaded mutable state — the whole window lives on one std
/// thread, and the WndProc callback is reentrant only via the same
/// thread's message pump. Using a `Mutex<State>` sidesteps the static-
/// mut lint without needing thread-local storage.
/// HWND isn't `Send` by default (it wraps a raw pointer); wrap in a
/// newtype with an explicit `unsafe impl` — we only touch it from the
/// window thread.
struct SendState(State);
unsafe impl Send for SendState {}

static STATE: OnceLock<Mutex<SendState>> = OnceLock::new();

fn state_set(s: State) {
    let _ = STATE.set(Mutex::new(SendState(s)));
}

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
    R: Default,
{
    match STATE.get() {
        Some(m) => match m.lock() {
            Ok(mut g) => f(&mut g.0),
            Err(p) => f(&mut p.into_inner().0),
        },
        None => R::default(),
    }
}

// ─── Fonts (recreated on DPI change) ───────────────────────────────────

// The window thread keeps a `Fonts` only for the initial handshake;
// real drawing creates+destroys font handles per-paint to stay simple.
// Font creation is fast (<1 ms) and the recovery window repaints rarely,
// so the churn is fine.
struct Fonts {
    title: HFONT,
    body: HFONT,
    button: HFONT,
    close: HFONT,
}

impl Fonts {
    fn create(scale: f32) -> Self {
        Self {
            title: make_font(14, FW_SEMIBOLD, scale),
            body: make_font(12, FW_NORMAL, scale),
            button: make_font(12, FW_SEMIBOLD, scale),
            close: make_font(18, FW_NORMAL, scale),
        }
    }
    fn destroy(&mut self) {
        unsafe {
            let _ = DeleteObject(self.title.into());
            let _ = DeleteObject(self.body.into());
            let _ = DeleteObject(self.button.into());
            let _ = DeleteObject(self.close.into());
        }
    }
}

fn make_font(
    point_size: i32,
    weight: windows::Win32::Graphics::Gdi::FONT_WEIGHT,
    scale: f32,
) -> HFONT {
    // CreateFontW's `cHeight` is in logical units; negative = point size.
    // DPI-scaled by our own scale factor because we opted the process into
    // PER_MONITOR_AWARE_V2 (Windows will not auto-scale for us).
    let height = -(((point_size as f32) * 96.0 / 72.0 * scale).round() as i32);
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        )
    }
}

// ─── Show / hide / dismiss ─────────────────────────────────────────────

unsafe fn present(hwnd: HWND, fonts: &Fonts, text: String) {
    let scale = window_scale(hwnd);

    // Layout pass 1: measure text height at our wrap width.
    let inner_w = (LOGICAL_WIDTH as f32 * scale) as i32 - (PADDING as f32 * 2.0 * scale) as i32;
    let hdc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
    let old_font = SelectObject(hdc, fonts.body.into());
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut measure = RECT {
        left: 0,
        top: 0,
        right: inner_w,
        bottom: 0,
    };
    if !wide.is_empty() {
        DrawTextW(
            hdc,
            &mut wide,
            &mut measure,
            DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX,
        );
    }
    let text_total_h = measure.bottom.max((BODY_LINE_HEIGHT as f32 * scale) as i32);
    let _ = SelectObject(hdc, old_font);
    let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), hdc);

    let max_body_h = (MAX_BODY_LINES as f32 * BODY_LINE_HEIGHT as f32 * scale) as i32;
    let body_h = text_total_h.min(max_body_h);

    let total_h = (PADDING as f32 * scale) as i32                      // top
        + (TITLE_HEIGHT as f32 * scale) as i32                         // title row
        + (PADDING as f32 * 0.5 * scale) as i32                        // title→body gap
        + body_h                                                        // body
        + (PADDING as f32 * scale) as i32                              // body→buttons gap
        + (BUTTON_HEIGHT as f32 * scale) as i32                        // button row
        + (PADDING as f32 * scale) as i32; // bottom

    let phys_w = (LOGICAL_WIDTH as f32 * scale) as i32;

    // Position on the active monitor, just above the overlay pill.
    let (mon_rect, _) = active_monitor_info();
    let mon_w = mon_rect.right - mon_rect.left;
    let x = mon_rect.left + (mon_w - phys_w) / 2;
    let bottom_margin = (BOTTOM_MARGIN as f32 * scale) as i32;
    let y = mon_rect.bottom - total_h - bottom_margin;

    SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        x,
        y,
        phys_w,
        total_h,
        SWP_NOACTIVATE,
    )
    .ok();

    // Record layout into state for the WM_PAINT pass and hit-testing.
    with_state::<_, ()>(|s| {
        s.visible = true;
        s.text = text;
        s.text_total_h = text_total_h;
        s.scale = scale;
        s.scroll_offset = 0;
        s.hot_copy = false;
        // Body rect (client coords).
        let padding = (PADDING as f32 * scale) as i32;
        let title_h = (TITLE_HEIGHT as f32 * scale) as i32;
        let title_gap = (PADDING as f32 * 0.5 * scale) as i32;
        let body_top = padding + title_h + title_gap;
        let body_right = phys_w - padding;
        s.body_rect = RECT {
            left: padding,
            top: body_top,
            right: body_right,
            bottom: body_top + body_h,
        };
        // Buttons — right-aligned.
        let btn_w = (BUTTON_WIDTH as f32 * scale) as i32;
        let btn_h = (BUTTON_HEIGHT as f32 * scale) as i32;
        let btn_top = total_h - padding - btn_h;
        s.btn_copy = RECT {
            left: body_right - btn_w,
            top: btn_top,
            right: body_right,
            bottom: btn_top + btn_h,
        };
        // Close ×, top-right corner.
        let close_size = (CLOSE_SIZE as f32 * scale) as i32;
        let close_margin = (PADDING as f32 * 0.6 * scale) as i32;
        s.btn_close = RECT {
            left: phys_w - close_margin - close_size,
            top: close_margin,
            right: phys_w - close_margin,
            bottom: close_margin + close_size,
        };
    });

    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = InvalidateRect(Some(hwnd), None, true);
}

unsafe fn dismiss(hwnd: HWND, user_initiated: bool) {
    let was_visible = with_state::<_, bool>(|s| {
        let prev = s.visible;
        s.visible = false;
        prev
    });
    if was_visible {
        let _ = ShowWindow(hwnd, SW_HIDE);
        emit(if user_initiated {
            RecoveryEvent::Dismissed
        } else {
            RecoveryEvent::Dismissed
        });
    }
}

unsafe fn emit_copy(hwnd: HWND) {
    // Read text out of state.
    let text = with_state::<_, String>(|s| s.text.clone());
    if let Err(e) = paste::copy_to_clipboard(&text) {
        log::error!("recovery copy failed: {e}");
    } else {
        log::info!(
            "recovery: copied {} chars to clipboard",
            text.chars().count()
        );
        emit(RecoveryEvent::Copied);
    }
    // Close regardless — user made their choice.
    dismiss(hwnd, false);
}

fn emit(event: RecoveryEvent) {
    if let Some(tx) = EVENT_TX.get() {
        let _ = tx.send(event);
    }
}

// ─── Window class & creation ───────────────────────────────────────────

unsafe fn register_window_class(h_instance: HINSTANCE) -> Result<()> {
    let class_name: Vec<u16> = "airtalkRecovery\0".encode_utf16().collect();
    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    let atom = RegisterClassExW(&wnd_class);
    if atom == 0 {
        let err = windows::Win32::Foundation::GetLastError();
        if err.0 != 1410 {
            // ERROR_CLASS_ALREADY_EXISTS
            bail!("RegisterClassExW(airtalkRecovery) failed: {err:?}");
        }
    }
    Ok(())
}

unsafe fn create_hidden_window(h_instance: HINSTANCE) -> Result<HWND> {
    let class_name: Vec<u16> = "airtalkRecovery\0".encode_utf16().collect();
    let win_name: Vec<u16> = "airtalk recovery\0".encode_utf16().collect();
    // Not layered — we're drawing with GDI and don't need per-pixel alpha.
    // NOACTIVATE + TOOLWINDOW keeps us out of Alt-Tab and prevents stealing
    // focus from the app the user is typing into.
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(win_name.as_ptr()),
        WS_POPUP,
        0,
        0,
        10,
        10,
        None,
        None,
        Some(h_instance),
        None,
    )
    .context("CreateWindowExW(recovery)")?;
    Ok(hwnd)
}

// ─── Monitor selection (same rule as overlay) ─────────────────────────

unsafe fn active_monitor_info() -> (RECT, f32) {
    let fg = GetForegroundWindow();
    let monitor = if fg.0.is_null() {
        MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY)
    } else {
        MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
    };
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let rect = if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    };
    let mut dx: u32 = 96;
    let mut dy: u32 = 96;
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    let scale = (dx as f32 / 96.0).max(1.0);
    (rect, scale)
}

unsafe fn window_scale(hwnd: HWND) -> f32 {
    let dpi = GetDpiForWindow(hwnd);
    if dpi == 0 {
        1.0
    } else {
        (dpi as f32 / 96.0).max(1.0)
    }
}

// ─── Window procedure ──────────────────────────────────────────────────

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1), // we paint the whole surface in WM_PAINT
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (l_param.0 & 0xFFFF) as i16 as i32;
            let y = ((l_param.0 >> 16) & 0xFFFF) as i16 as i32;
            handle_click(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((w_param.0 >> 16) & 0xFFFF) as i16 as i32;
            handle_wheel(hwnd, delta);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if w_param.0 as u16 == VK_ESCAPE.0 {
                dismiss(hwnd, true);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // Rebuild fonts on DPI transition when dragged across monitors.
            // (Unlikely for this popup, which is small and auto-positioned,
            //  but handles the edge case where a monitor arrangement changes
            //  between show() and dismiss().)
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

fn handle_click(hwnd: HWND, x: i32, y: i32) {
    let (copy_rect, close_rect) = with_state::<_, (RECT, RECT)>(|s| (s.btn_copy, s.btn_close));
    if point_in(x, y, &close_rect) {
        unsafe { dismiss(hwnd, true) };
        return;
    }
    if point_in(x, y, &copy_rect) {
        unsafe { emit_copy(hwnd) };
    }
}

fn handle_wheel(hwnd: HWND, delta: i32) {
    with_state::<_, ()>(|s| {
        let body_h = s.body_rect.bottom - s.body_rect.top;
        let max_scroll = (s.text_total_h - body_h).max(0);
        // Windows sends ±120 per notch. Tuning: 1 notch = ~2 lines.
        let line_h = (BODY_LINE_HEIGHT as f32 * s.scale) as i32;
        let step = (delta / 120) * line_h * 2;
        s.scroll_offset = (s.scroll_offset - step).clamp(0, max_scroll);
    });
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn point_in(x: i32, y: i32, r: &RECT) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ─── Painting ──────────────────────────────────────────────────────────

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut client: RECT = std::mem::zeroed();
    let _ = GetClientRect(hwnd, &mut client);

    // Snapshot the bits of state we need — avoid holding the mutex across GDI calls.
    let (
        visible,
        text,
        text_total_h,
        scroll_offset,
        body_rect,
        btn_copy,
        btn_close,
        hot_copy,
        scale,
    ) = with_state::<_, (bool, String, i32, i32, RECT, RECT, RECT, bool, f32)>(|s| {
        (
            s.visible,
            s.text.clone(),
            s.text_total_h,
            s.scroll_offset,
            s.body_rect,
            s.btn_copy,
            s.btn_close,
            s.hot_copy,
            s.scale,
        )
    });
    if !visible {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 1. Background fill.
    let bg = CreateSolidBrush(colorref(BG_COLOR));
    let _ = FillRect(hdc, &client, bg);
    let _ = DeleteObject(bg.into());

    // 2. Title "自动贴入失败".
    let fonts_title = make_font(14, FW_SEMIBOLD, scale);
    let fonts_body = make_font(12, FW_NORMAL, scale);
    let fonts_btn = make_font(12, FW_SEMIBOLD, scale);
    let fonts_close = make_font(16, FW_NORMAL, scale);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, colorref(ACCENT_COLOR));
    let title_rect = RECT {
        left: (PADDING as f32 * scale) as i32,
        top: (PADDING as f32 * scale) as i32,
        right: client.right - (PADDING as f32 * scale) as i32,
        bottom: (PADDING as f32 * scale) as i32 + (TITLE_HEIGHT as f32 * scale) as i32,
    };
    let old_font = SelectObject(hdc, fonts_title.into());
    let mut title_rect_copy = title_rect;
    let mut title_w: Vec<u16> = "自动贴入失败".encode_utf16().collect();
    DrawTextW(
        hdc,
        &mut title_w,
        &mut title_rect_copy,
        DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );

    // 3. Body (scrollable).
    SetTextColor(hdc, colorref(FG_COLOR));
    let _ = SelectObject(hdc, fonts_body.into());
    // Draw into a virtual rect offset by -scroll_offset, clipped to body_rect.
    let save_dc = windows::Win32::Graphics::Gdi::SaveDC(hdc);
    let _ = windows::Win32::Graphics::Gdi::IntersectClipRect(
        hdc,
        body_rect.left,
        body_rect.top,
        body_rect.right,
        body_rect.bottom,
    );
    let mut body_wide: Vec<u16> = text.encode_utf16().collect();
    let mut body_draw = RECT {
        left: body_rect.left,
        top: body_rect.top - scroll_offset,
        right: body_rect.right,
        bottom: body_rect.top - scroll_offset + text_total_h,
    };
    if !body_wide.is_empty() {
        DrawTextW(
            hdc,
            &mut body_wide,
            &mut body_draw,
            DT_WORDBREAK | DT_NOPREFIX,
        );
    }
    let _ = windows::Win32::Graphics::Gdi::RestoreDC(hdc, save_dc);

    // 3b. Subtle scroll indicator (thin vertical bar on the right edge of body)
    let body_h = body_rect.bottom - body_rect.top;
    if text_total_h > body_h {
        let max_scroll = text_total_h - body_h;
        let bar_h = (body_h * body_h / text_total_h).max((8.0 * scale) as i32);
        let bar_y = body_rect.top
            + ((body_h - bar_h) as f32 * (scroll_offset as f32 / max_scroll as f32)) as i32;
        let bar_w = (3.0 * scale) as i32;
        let bar_x = body_rect.right - bar_w - 1;
        let bar_rect = RECT {
            left: bar_x,
            top: bar_y,
            right: bar_x + bar_w,
            bottom: bar_y + bar_h,
        };
        let bar_brush = CreateSolidBrush(colorref(FG_SUBTLE));
        let _ = FillRect(hdc, &bar_rect, bar_brush);
        let _ = DeleteObject(bar_brush.into());
    }

    // 4. Copy button.
    let btn_color = if hot_copy { BUTTON_BG_HOT } else { BUTTON_BG };
    let btn_brush = CreateSolidBrush(colorref(btn_color));
    let _ = FillRect(hdc, &btn_copy, btn_brush);
    let _ = DeleteObject(btn_brush.into());
    SetTextColor(hdc, colorref(FG_COLOR));
    let _ = SelectObject(hdc, fonts_btn.into());
    let mut btn_rect_copy = btn_copy;
    let mut btn_label: Vec<u16> = "复制".encode_utf16().collect();
    DrawTextW(
        hdc,
        &mut btn_label,
        &mut btn_rect_copy,
        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
    );

    // 5. Close × (no background fill, just the glyph).
    let _ = SelectObject(hdc, fonts_close.into());
    SetTextColor(hdc, colorref(FG_SUBTLE));
    let mut close_copy = btn_close;
    let mut close_glyph: Vec<u16> = "×".encode_utf16().collect();
    DrawTextW(
        hdc,
        &mut close_glyph,
        &mut close_copy,
        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
    );

    // Restore default font before cleanup.
    let _ = SelectObject(hdc, old_font);

    let _ = DeleteObject(fonts_title.into());
    let _ = DeleteObject(fonts_body.into());
    let _ = DeleteObject(fonts_btn.into());
    let _ = DeleteObject(fonts_close.into());

    let _ = EndPaint(hwnd, &ps);
}
