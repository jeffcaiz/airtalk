//! Bottom-centered floating status pill. Layered + click-through + topmost.
//!
//! **No text.** airtalk doesn't stream interim text, doesn't display LLM
//! diffs, and doesn't show state labels like "Listening…" / "Processing…".
//! The overlay conveys state purely through shape and animation:
//!
//!   * [`OverlayState::Idle`]       — fully transparent (hidden).
//!   * [`OverlayState::Recording`]  — 5-bar waveform, driven by the
//!                                     audio thread's RMS atomic with a
//!                                     small phase-offset per bar so each
//!                                     dances slightly differently.
//!   * [`OverlayState::Processing`] — 3 bouncing dots.
//!   * [`OverlayState::Success`]    — green ✓ with a stroke-in intro,
//!                                     auto-returns to Idle after ~800 ms.
//!                                     Fires after a successful paste.
//!   * [`OverlayState::Error`]      — red X, auto-returns to Idle after
//!                                     ~800 ms.
//!
//! Rendering: pure-Rust via [`tiny_skia`] into a premultiplied-RGBA
//! `Pixmap`. Each frame we byte-swap R↔B into a DIB section and blit
//! to the screen with `UpdateLayeredWindow(ULW_ALPHA)` — no Direct2D,
//! no COM, no DWM dance.
//!
//! Architecture: one dedicated std::thread owns the HWND and runs a
//! `PeekMessage` + `try_recv` + render loop at ~60 fps while visible,
//! ~6 fps while fully idle. Commands from the UI thread flow in via
//! a `std::sync::mpsc` channel.

#![cfg(windows)]

use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HDC, HGDIOBJ, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetSystemMetrics, LoadCursorW, PeekMessageW, PostQuitMessage, RegisterClassExW, SetWindowPos,
    ShowWindow, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, IDC_ARROW, MSG,
    PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_DESTROY,
    WM_QUIT, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

// ─── Layout ────────────────────────────────────────────────────────────

// Logical units (100% DPI). Physical pixel dimensions are computed at
// runtime as `logical * scale` where `scale = dpi / 96.0` for the
// monitor the overlay currently sits on.
const LOGICAL_WIDTH: u32 = 180;
const LOGICAL_HEIGHT: u32 = 48;
const LOGICAL_BOTTOM_MARGIN: i32 = 48;

// ─── Colors (matched to koe-shell) ─────────────────────────────────────
//
// koe's overlay.rs constants (lines 171–282). Keeping them verbatim
// here so both apps look familiar side-by-side.

// Pill background: near-black charcoal at 88% opacity.
const BG_RGBA: (u8, u8, u8, u8) = (28, 25, 25, 224); // #1c1919 @ ~0.88

// Subtle white border at 8% alpha, 1 px wide.
const BORDER_ALPHA: f32 = 0.08;

// Accent colors per state. f32 in [0,1] so alpha modulation is cheap.
const RECORDING_RGB: [f32; 3] = [1.0, 0.32, 0.32]; // red — matches Error
const PROCESSING_RGB: [f32; 3] = [0.35, 0.78, 1.0]; // light blue
                                                    // Cool teal-leaning green — reads "done" against the recording red and
                                                    // processing blue without the highlighter-pen vibe of a pure #4ade80.
                                                    // Roughly #38d699.
const SUCCESS_RGB: [f32; 3] = [0.22, 0.84, 0.60];
const ERROR_RGB: [f32; 3] = [1.0, 0.32, 0.32]; // same red as recording

// ─── Public API ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    Idle,
    Recording,
    Processing,
    Success,
    Error,
}

pub struct Overlay {
    cmd_tx: mpsc::Sender<Command>,
    _thread: JoinHandle<()>,
}

/// Cheap clonable handle for setting overlay state from worker tasks
/// (e.g. the paste `spawn_blocking`) without passing `&Overlay` around.
#[derive(Clone)]
pub struct OverlayHandle {
    cmd_tx: mpsc::Sender<Command>,
}

impl OverlayHandle {
    pub fn set_state(&self, state: OverlayState) {
        let _ = self.cmd_tx.send(Command::SetState(state));
    }
}

enum Command {
    SetState(OverlayState),
}

impl Overlay {
    pub fn start(level_source: Arc<AtomicU32>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();

        let thread = std::thread::Builder::new()
            .name("airtalk-overlay".into())
            .spawn(move || {
                if let Err(e) = run_overlay_thread(cmd_rx, level_source, &init_tx) {
                    let _ = init_tx.send(Err(e));
                }
            })
            .context("spawn overlay thread")?;

        init_rx
            .recv_timeout(Duration::from_secs(2))
            .context("overlay init timeout (2s)")?
            .context("overlay init error")?;

        Ok(Self {
            cmd_tx,
            _thread: thread,
        })
    }

    pub fn set_state(&self, state: OverlayState) {
        let _ = self.cmd_tx.send(Command::SetState(state));
    }

    pub fn handle(&self) -> OverlayHandle {
        OverlayHandle {
            cmd_tx: self.cmd_tx.clone(),
        }
    }
}

// ─── Thread ────────────────────────────────────────────────────────────

fn run_overlay_thread(
    cmd_rx: mpsc::Receiver<Command>,
    level: Arc<AtomicU32>,
    init_tx: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    unsafe {
        let h_instance = GetModuleHandleW(None).context("GetModuleHandleW")?;
        register_window_class(h_instance.into())?;
        let hwnd = create_overlay_window(h_instance.into())?;

        let screen_dc = GetDC(None);

        // Start with primary monitor's scale; will be updated on the
        // first Idle → active transition via reposition_and_resize().
        let (_, initial_scale) = active_monitor_info();
        let mut target = RenderTarget::create(screen_dc, initial_scale)?;
        reposition_and_resize(hwnd, &target);

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let _ = init_tx.send(Ok(()));

        let mut state = OverlayState::Idle;
        let mut state_at = Instant::now();
        let mut visible_alpha: f32 = 0.0;
        let mut last_frame = Instant::now();
        let anim_start = Instant::now();

        loop {
            // Drain any pending window messages. We don't take input but
            // the shell may still post us DWM / display / shutdown messages.
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, Some(HWND::default()), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    target.destroy();
                    cleanup_window(hwnd, screen_dc);
                    return Ok(());
                }
                DispatchMessageW(&msg);
            }

            // Drain commands from the UI thread.
            loop {
                match cmd_rx.try_recv() {
                    Ok(Command::SetState(s)) => {
                        if s != state {
                            let was_idle = matches!(state, OverlayState::Idle);
                            let becoming_active = !matches!(s, OverlayState::Idle);
                            state = s;
                            state_at = Instant::now();
                            // When the pill is about to "appear" (Idle → something),
                            // follow the focused window to its monitor AND pick up
                            // that monitor's DPI. Stable within a recording session —
                            // repositioning mid-session would feel jumpy if the user
                            // Alt-Tabs to another display.
                            if was_idle && becoming_active {
                                let (_, new_scale) = active_monitor_info();
                                if (new_scale - target.scale).abs() > 0.01 {
                                    target.destroy();
                                    target = RenderTarget::create(screen_dc, new_scale)?;
                                }
                                reposition_and_resize(hwnd, &target);
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        target.destroy();
                        cleanup_window(hwnd, screen_dc);
                        return Ok(());
                    }
                }
            }

            // Auto-advance Success / Error → Idle after their flash.
            // Success lingers a bit longer so the check feels like a
            // confirmation rather than a blink.
            let auto_advance_after = match state {
                OverlayState::Success => Some(Duration::from_millis(1100)),
                OverlayState::Error => Some(Duration::from_millis(800)),
                _ => None,
            };
            if let Some(d) = auto_advance_after {
                if state_at.elapsed() > d {
                    state = OverlayState::Idle;
                    state_at = Instant::now();
                }
            }

            // Fade envelope.
            let now = Instant::now();
            let dt = (now - last_frame).as_secs_f32().min(0.1);
            last_frame = now;
            let target_alpha = if matches!(state, OverlayState::Idle) {
                0.0
            } else {
                1.0
            };
            // k = speed; higher = snappier. Separate rates for in vs out.
            let k = if target_alpha > visible_alpha {
                9.0
            } else {
                6.0
            };
            visible_alpha = ease_toward(visible_alpha, target_alpha, k, dt);

            // Render into pixmap.
            let rms = f32::from_bits(level.load(Ordering::Acquire));
            render(
                &mut target.pixmap,
                state,
                state_at.elapsed(),
                anim_start.elapsed().as_secs_f32(),
                rms,
                visible_alpha,
                target.scale,
            );

            // Copy pixmap (RGBA premultiplied) → DIB (BGRA premultiplied).
            copy_pixmap_to_dib(
                target.pixmap.data(),
                target.dib_bits,
                target.physical_w,
                target.physical_h,
            );

            // Blit.
            push_layered(
                hwnd,
                screen_dc,
                target.mem_dc,
                target.physical_w,
                target.physical_h,
            );

            // Sleep. When invisible and Idle, back off to save cycles.
            let sleep = if visible_alpha <= 0.002 && matches!(state, OverlayState::Idle) {
                Duration::from_millis(160)
            } else {
                Duration::from_millis(16) // ~60 fps
            };
            std::thread::sleep(sleep);
        }
    }
}

// ─── Rendering ────────────────────────────────────────────────────────

fn render(
    pixmap: &mut Pixmap,
    state: OverlayState,
    state_elapsed: Duration,
    anim_t: f32,
    rms: f32,
    visible_alpha: f32,
    scale: f32,
) {
    pixmap.fill(Color::TRANSPARENT);
    if visible_alpha <= 0.002 {
        return;
    }

    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;

    draw_pill_background(pixmap, w, h, visible_alpha, scale);

    match state {
        OverlayState::Recording => draw_waveform(pixmap, w, h, anim_t, rms, visible_alpha, scale),
        OverlayState::Processing => {
            draw_processing_dots(pixmap, w, h, anim_t, visible_alpha, scale)
        }
        OverlayState::Success => {
            draw_success_check(pixmap, w, h, state_elapsed, visible_alpha, scale)
        }
        OverlayState::Error => draw_error_x(pixmap, w, h, state_elapsed, visible_alpha, scale),
        OverlayState::Idle => {}
    }
}

fn draw_pill_background(pixmap: &mut Pixmap, w: f32, h: f32, alpha: f32, scale: f32) {
    // Inset by 0.5 * scale so the border's stroke sits fully inside the bitmap.
    let inset = 0.5 * scale;
    let x = inset;
    let y = inset;
    let pw = w - 2.0 * inset;
    let ph = h - 2.0 * inset;

    let mut pb = PathBuilder::new();
    push_pill(&mut pb, x, y, pw, ph);
    let path = pb.finish().expect("background pill path");

    // Fill.
    let mut fill = Paint::default();
    fill.set_color(scale_alpha(
        Color::from_rgba8(BG_RGBA.0, BG_RGBA.1, BG_RGBA.2, BG_RGBA.3),
        alpha,
    ));
    fill.anti_alias = true;
    pixmap.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);

    // Subtle white border.
    let mut border = Paint::default();
    border.set_color(
        Color::from_rgba(1.0, 1.0, 1.0, BORDER_ALPHA * alpha).unwrap_or(Color::TRANSPARENT),
    );
    border.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 1.0 * scale;
    pixmap.stroke_path(&path, &border, &stroke, Transform::identity(), None);
}

fn draw_waveform(
    pixmap: &mut Pixmap,
    w: f32,
    h: f32,
    anim_t: f32,
    rms: f32,
    alpha: f32,
    scale: f32,
) {
    const BAR_COUNT: usize = 5;
    let bar_width = 7.0 * scale;
    let bar_gap = 5.0 * scale;

    // Amplify RMS: speech is usually ~0.02..0.1, map to 0..1.
    let rms_boosted = (rms * 8.0).clamp(0.0, 1.0);
    // Always a subtle idle animation so the overlay feels "alive" even
    // in silence — mirrors koe's behavior at near-zero level.
    let idle_breath = 0.18;

    let total = BAR_COUNT as f32 * bar_width + (BAR_COUNT as f32 - 1.0) * bar_gap;
    let start_x = (w - total) / 2.0;
    let band_top = h * 0.22;
    let band_bot = h * 0.78;
    let band_h = band_bot - band_top;
    let y_center = (band_top + band_bot) / 2.0;
    let min_bar_h = bar_width; // pill geometry: height ≥ width

    for i in 0..BAR_COUNT {
        let phase = i as f32 * 0.9;
        let osc = 0.5 + 0.5 * (anim_t * 3.2 + phase).sin();
        let level_here = (idle_breath * osc + rms_boosted * (0.4 + 0.6 * osc)).clamp(0.0, 1.0);
        let bar_h = (band_h * level_here).max(min_bar_h);
        let x = start_x + i as f32 * (bar_width + bar_gap);
        let y = y_center - bar_h / 2.0;

        // Per-bar alpha keyed to height: taller bar = more opaque. Matches
        // koe's 0.45..1.0 range so quiet bars recede.
        let bar_alpha = 0.45 + 0.55 * level_here;
        let color = Color::from_rgba(
            RECORDING_RGB[0],
            RECORDING_RGB[1],
            RECORDING_RGB[2],
            bar_alpha * alpha,
        )
        .unwrap_or_else(|| Color::from_rgba8(255, 82, 82, 255));

        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;

        let mut pb = PathBuilder::new();
        push_pill(&mut pb, x, y, bar_width, bar_h);
        if let Some(path) = pb.finish() {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_processing_dots(pixmap: &mut Pixmap, w: f32, h: f32, anim_t: f32, alpha: f32, scale: f32) {
    const DOT_COUNT: usize = 3;
    let dot_radius = 4.0 * scale;
    let dot_gap = 10.0 * scale;
    let bob_amp = 6.0 * scale;

    let total = DOT_COUNT as f32 * (dot_radius * 2.0) + (DOT_COUNT as f32 - 1.0) * dot_gap;
    let start_x = (w - total) / 2.0 + dot_radius;
    let y_center = h / 2.0;

    for i in 0..DOT_COUNT {
        let phase = i as f32 * (2.0 * PI / 6.0);
        let bob = (anim_t * 5.0 + phase).sin();
        let cx = start_x + i as f32 * (dot_radius * 2.0 + dot_gap);
        let cy = y_center - bob * bob_amp;

        // Alpha follows the sine too (high dot = opaque, low dot = dim),
        // matching koe's 0.35..1.0 animated alpha on processing dots.
        let dot_alpha = 0.35 + 0.65 * ((bob + 1.0) / 2.0);
        let color = Color::from_rgba(
            PROCESSING_RGB[0],
            PROCESSING_RGB[1],
            PROCESSING_RGB[2],
            dot_alpha * alpha,
        )
        .unwrap_or_else(|| Color::from_rgba8(89, 199, 255, 255));

        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, dot_radius);
        if let Some(path) = pb.finish() {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_success_check(
    pixmap: &mut Pixmap,
    w: f32,
    h: f32,
    state_elapsed: Duration,
    alpha: f32,
    scale: f32,
) {
    // Checkmark geometry. Two connected segments: a short down-slope
    // into p2 (the "elbow"), then a long up-slope out to p3.
    let r = 13.0 * scale;
    let cx = w / 2.0;
    let cy = h / 2.0;
    let p1 = (cx - r * 0.70, cy - r * 0.05);
    let p2 = (cx - r * 0.18, cy + r * 0.45);
    let p3 = (cx + r * 0.72, cy - r * 0.48);

    // Stroke-in over the first ~320 ms, with an ease-out curve so the pen
    // "lands" decisively instead of crawling linearly. At t=0 we render a
    // zero-length path, which tiny-skia renders as a round dot (stroke cap)
    // — a nice seed for the animation to grow from.
    let t_raw = (state_elapsed.as_secs_f32() / 0.32).clamp(0.0, 1.0);
    let t = 1.0 - (1.0 - t_raw).powi(2);
    let len_short = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
    let len_long = ((p3.0 - p2.0).powi(2) + (p3.1 - p2.1).powi(2)).sqrt();
    let drawn = (len_short + len_long) * t;

    let mut pb = PathBuilder::new();
    pb.move_to(p1.0, p1.1);
    if drawn <= len_short {
        let k = if len_short > 0.0 {
            drawn / len_short
        } else {
            1.0
        };
        pb.line_to(p1.0 + (p2.0 - p1.0) * k, p1.1 + (p2.1 - p1.1) * k);
    } else {
        pb.line_to(p2.0, p2.1);
        let k = if len_long > 0.0 {
            ((drawn - len_short) / len_long).clamp(0.0, 1.0)
        } else {
            1.0
        };
        pb.line_to(p2.0 + (p3.0 - p2.0) * k, p2.1 + (p3.1 - p2.1) * k);
    }

    let mut paint = Paint::default();
    paint.set_color(
        Color::from_rgba(SUCCESS_RGB[0], SUCCESS_RGB[1], SUCCESS_RGB[2], alpha)
            .unwrap_or_else(|| Color::from_rgba8(56, 214, 153, 255)),
    );
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 3.5 * scale;
    stroke.line_cap = tiny_skia::LineCap::Round;
    stroke.line_join = tiny_skia::LineJoin::Round;

    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_error_x(
    pixmap: &mut Pixmap,
    w: f32,
    h: f32,
    state_elapsed: Duration,
    alpha: f32,
    scale: f32,
) {
    // Ease from 0 to full size over first 120ms so it "pops" in.
    let intro = (state_elapsed.as_secs_f32() / 0.12).clamp(0.0, 1.0);
    let r = 10.0 * scale * intro;
    let cx = w / 2.0;
    let cy = h / 2.0;
    let mut paint = Paint::default();
    paint.set_color(
        Color::from_rgba(ERROR_RGB[0], ERROR_RGB[1], ERROR_RGB[2], alpha)
            .unwrap_or_else(|| Color::from_rgba8(255, 82, 82, 255)),
    );
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 3.0 * scale;
    stroke.line_cap = tiny_skia::LineCap::Round;

    let mut pb = PathBuilder::new();
    pb.move_to(cx - r, cy - r);
    pb.line_to(cx + r, cy + r);
    pb.move_to(cx + r, cy - r);
    pb.line_to(cx - r, cy + r);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Push a stadium/pill path. Auto-detects orientation from aspect ratio.
fn push_pill(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    if w >= h {
        push_pill_horizontal(pb, x, y, w, h);
    } else {
        push_pill_vertical(pb, x, y, w, h);
    }
}

fn push_pill_horizontal(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    let r = h / 2.0;
    let left_cx = x + r;
    let right_cx = x + w - r;
    pb.move_to(left_cx, y);
    pb.line_to(right_cx, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.quad_to(x + w, y + h, right_cx, y + h);
    pb.line_to(left_cx, y + h);
    pb.quad_to(x, y + h, x, y + r);
    pb.quad_to(x, y, left_cx, y);
    pb.close();
}

fn push_pill_vertical(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    let r = w / 2.0;
    let top_cy = y + r;
    let bot_cy = y + h - r;
    pb.move_to(x, top_cy);
    pb.line_to(x, bot_cy);
    pb.quad_to(x, y + h, x + r, y + h);
    pb.quad_to(x + w, y + h, x + w, bot_cy);
    pb.line_to(x + w, top_cy);
    pb.quad_to(x + w, y, x + r, y);
    pb.quad_to(x, y, x, top_cy);
    pb.close();
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn scale_alpha(color: Color, alpha: f32) -> Color {
    let a = (color.alpha() * alpha).clamp(0.0, 1.0);
    Color::from_rgba(color.red(), color.green(), color.blue(), a).unwrap_or(color)
}

fn ease_toward(current: f32, target: f32, k: f32, dt: f32) -> f32 {
    let step = (k * dt).clamp(0.0, 1.0);
    current + (target - current) * step
}

fn copy_pixmap_to_dib(src: &[u8], dst_bits: *mut std::ffi::c_void, w: u32, h: u32) {
    if dst_bits.is_null() {
        return;
    }
    let pixel_count = (w * h) as usize;
    let dst = unsafe { std::slice::from_raw_parts_mut(dst_bits as *mut u8, pixel_count * 4) };
    // tiny-skia: RGBA premultiplied. Windows layered DIB: BGRA premultiplied.
    // Swap R↔B per pixel.
    for i in 0..pixel_count {
        let so = i * 4;
        let dst_o = i * 4;
        dst[dst_o] = src[so + 2];
        dst[dst_o + 1] = src[so + 1];
        dst[dst_o + 2] = src[so];
        dst[dst_o + 3] = src[so + 3];
    }
}

// ─── Render target (DIB + Pixmap) ──────────────────────────────────────

/// Owns the Win32 GDI + tiny-skia resources we rebuild on DPI change.
struct RenderTarget {
    mem_dc: HDC,
    old_obj: HGDIOBJ,
    dib_handle: HGDIOBJ,
    dib_bits: *mut std::ffi::c_void,
    pixmap: Pixmap,
    physical_w: u32,
    physical_h: u32,
    scale: f32,
}

impl RenderTarget {
    unsafe fn create(screen_dc: HDC, scale: f32) -> Result<Self> {
        let physical_w = ((LOGICAL_WIDTH as f32 * scale).round() as u32).max(1);
        let physical_h = ((LOGICAL_HEIGHT as f32 * scale).round() as u32).max(1);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let (dib_handle, dib_bits) = create_dib_section(mem_dc, physical_w, physical_h)?;
        let old_obj = SelectObject(mem_dc, dib_handle);
        let pixmap = Pixmap::new(physical_w, physical_h)
            .ok_or_else(|| anyhow!("Pixmap::new({physical_w},{physical_h}) failed"))?;
        Ok(Self {
            mem_dc,
            old_obj,
            dib_handle,
            dib_bits,
            pixmap,
            physical_w,
            physical_h,
            scale,
        })
    }

    unsafe fn destroy(&mut self) {
        let _ = SelectObject(self.mem_dc, self.old_obj);
        let _ = DeleteObject(self.dib_handle);
        let _ = DeleteDC(self.mem_dc);
    }
}

// ─── Win32 boilerplate ─────────────────────────────────────────────────

unsafe fn register_window_class(h_instance: HINSTANCE) -> Result<()> {
    let class_name: Vec<u16> = "airtalkOverlay\0".encode_utf16().collect();
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
        // ERROR_CLASS_ALREADY_EXISTS (1410) is fine — we've registered before.
        let err = windows::Win32::Foundation::GetLastError();
        if err.0 != 1410 {
            bail!("RegisterClassExW failed: {err:?}");
        }
    }
    Ok(())
}

unsafe fn create_overlay_window(h_instance: HINSTANCE) -> Result<HWND> {
    let class_name: Vec<u16> = "airtalkOverlay\0".encode_utf16().collect();
    let window_name: Vec<u16> = "airtalk\0".encode_utf16().collect();

    // Start at origin with logical size; reposition_and_resize will
    // move and scale us before the first blit.
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(window_name.as_ptr()),
        WS_POPUP,
        0,
        0,
        LOGICAL_WIDTH as i32,
        LOGICAL_HEIGHT as i32,
        None,
        None,
        Some(h_instance),
        None,
    )
    .context("CreateWindowExW")?;

    Ok(hwnd)
}

/// Find the monitor that contains the foreground window (or primary if
/// nothing's focused) and return both its work rect and its DPI scale.
/// The scale is clamped to ≥ 1.0 — we never shrink below logical size.
unsafe fn active_monitor_info() -> (RECT, f32) {
    let fg = GetForegroundWindow();
    let monitor = if fg.0.is_null() {
        MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY)
    } else {
        MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
    };
    let rect = monitor_work_rect(monitor);
    let scale = monitor_scale(monitor);
    (rect, scale)
}

unsafe fn monitor_work_rect(monitor: HMONITOR) -> RECT {
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

unsafe fn monitor_scale(monitor: HMONITOR) -> f32 {
    let mut dpi_x: u32 = 96;
    let mut dpi_y: u32 = 96;
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
        (dpi_x as f32 / 96.0).max(1.0)
    } else {
        1.0
    }
}

/// Re-home the overlay to the active monitor's work rect, using the
/// current render target's physical size (which reflects the monitor's
/// DPI scale). Called on every Idle → active transition.
unsafe fn reposition_and_resize(hwnd: HWND, target: &RenderTarget) {
    let (r, _) = active_monitor_info();
    let mon_w = r.right - r.left;
    let x = r.left + (mon_w - target.physical_w as i32) / 2;
    let bottom_margin = (LOGICAL_BOTTOM_MARGIN as f32 * target.scale) as i32;
    let y = r.bottom - target.physical_h as i32 - bottom_margin;
    // Note: no SWP_NOSIZE — width/height may change if scale changed.
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        x,
        y,
        target.physical_w as i32,
        target.physical_h as i32,
        SWP_NOACTIVATE,
    );
}

unsafe fn create_dib_section(
    mem_dc: HDC,
    w: u32,
    h: u32,
) -> Result<(HGDIOBJ, *mut std::ffi::c_void)> {
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // negative → top-down pixel order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default(); 1],
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(Some(mem_dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0)
        .context("CreateDIBSection")?;
    Ok((dib.into(), bits))
}

unsafe fn push_layered(hwnd: HWND, screen_dc: HDC, mem_dc: HDC, w: u32, h: u32) {
    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE {
        cx: w as i32,
        cy: h as i32,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        Some(screen_dc),
        None,
        Some(&sz),
        Some(mem_dc),
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
}

unsafe fn cleanup_window(hwnd: HWND, screen_dc: HDC) {
    ReleaseDC(None, screen_dc);
    let _ = DestroyWindow(hwnd);
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}
