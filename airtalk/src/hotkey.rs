//! Global hotkey via `WH_KEYBOARD_LL` (Win32 low-level keyboard hook).
//!
//! Runs a dedicated thread that installs the hook and pumps the Win32
//! message loop. The hook callback:
//!
//!   1. Matches the key against the configured trigger. Uses the VK
//!      codes — low-level hooks already distinguish left/right
//!      modifiers (`VK_LMENU` vs `VK_RMENU`), so we don't need to
//!      check the extended flag for Alt/Ctrl/Shift.
//!   2. Updates pressed / recording state.
//!   3. Posts a [`HotkeyEvent`] through an unbounded mpsc.
//!   4. **Returns `LRESULT(1)` to swallow the key** — this is the
//!      reason we use the low-level hook instead of `rdev` /
//!      `RegisterHotKey`: it's the only way to stop Alt taps from
//!      reaching the focused window and activating the F10 menu.
//!
//! Only one [`Hotkey`] instance per process. `start` returns `Err` if
//! called a second time.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_CAPITAL, VK_F1, VK_F10, VK_F11, VK_F12, VK_F13, VK_F14, VK_F15, VK_F16, VK_F17, VK_F18,
    VK_F19, VK_F2, VK_F20, VK_F21, VK_F22, VK_F23, VK_F24, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7,
    VK_F8, VK_F9, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HHOOK,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// Events emitted by the hotkey engine. Press starts a recording cycle;
/// Release ends it. The two are always balanced (never two Presses in a
/// row from the caller's perspective).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
}

/// Interaction model.
///
/// * **Combo** (default) — merges hold and tap. If you release the key
///   within [`COMBO_HOLD_MS`] ms the recording keeps running; tap again
///   to stop. If you hold past that threshold, releasing stops immediately
///   (push-to-talk). This matches koe-shell's behavior and is what most
///   users expect.
/// * **Hold** — strict push-to-talk. Release always stops, regardless of
///   how long you held.
/// * **Tap** — strict toggle. Each tap flips between recording / idle.
///   Release is a no-op.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Combo,
    Hold,
    Tap,
}

/// Threshold (ms) below which a key-up in [`Mode::Combo`] is treated as
/// a short tap (keep recording, wait for next tap to stop) rather than
/// a push-to-talk release.
pub const COMBO_HOLD_MS: u128 = 300;

// Only RightAlt is wired into Config::default() so far; the rest sit on
// the trigger_vk table ready for settings UI to surface them later.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    RightAlt,
    LeftAlt,
    RightCtrl,
    LeftCtrl,
    RightShift,
    LeftShift,
    RightWin,
    LeftWin,
    CapsLock,
    /// Function keys F1..=F24. Values outside that range are rejected
    /// at `start` time.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub trigger: Trigger,
    pub mode: Mode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger: Trigger::RightAlt,
            mode: Mode::Combo,
        }
    }
}

/// Convert a [`Trigger`] to its VK code. Returns `None` for
/// out-of-range `F(n)`.
fn trigger_vk(t: Trigger) -> Option<u32> {
    let vk = match t {
        Trigger::RightAlt => VK_RMENU.0,
        Trigger::LeftAlt => VK_LMENU.0,
        Trigger::RightCtrl => VK_RCONTROL.0,
        Trigger::LeftCtrl => VK_LCONTROL.0,
        Trigger::RightShift => VK_RSHIFT.0,
        Trigger::LeftShift => VK_LSHIFT.0,
        Trigger::RightWin => VK_RWIN.0,
        Trigger::LeftWin => VK_LWIN.0,
        Trigger::CapsLock => VK_CAPITAL.0,
        Trigger::F(n) => match n {
            1 => VK_F1.0,
            2 => VK_F2.0,
            3 => VK_F3.0,
            4 => VK_F4.0,
            5 => VK_F5.0,
            6 => VK_F6.0,
            7 => VK_F7.0,
            8 => VK_F8.0,
            9 => VK_F9.0,
            10 => VK_F10.0,
            11 => VK_F11.0,
            12 => VK_F12.0,
            13 => VK_F13.0,
            14 => VK_F14.0,
            15 => VK_F15.0,
            16 => VK_F16.0,
            17 => VK_F17.0,
            18 => VK_F18.0,
            19 => VK_F19.0,
            20 => VK_F20.0,
            21 => VK_F21.0,
            22 => VK_F22.0,
            23 => VK_F23.0,
            24 => VK_F24.0,
            _ => return None,
        },
    };
    Some(vk as u32)
}

pub struct Hotkey {
    events: mpsc::UnboundedReceiver<HotkeyEvent>,
    // Thread parks on GetMessage forever; dropping the join handle
    // detaches it. Process exit (or Job Object) will reap it.
    _thread: JoinHandle<()>,
}

impl Hotkey {
    pub fn start(config: Config) -> Result<Self> {
        let trigger_vk = trigger_vk(config.trigger)
            .ok_or_else(|| anyhow!("invalid trigger: {:?}", config.trigger))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let state = HookState {
            trigger_vk,
            mode: config.mode,
            is_pressed: AtomicBool::new(false),
            logical: Mutex::new(LogicalState::Idle),
            tx,
        };
        HOOK_STATE
            .set(state)
            .map_err(|_| anyhow!("Hotkey::start already called in this process"))?;

        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("airtalk-hotkey".into())
            .spawn(move || {
                run_hook_thread(init_tx);
            })
            .context("spawn hotkey thread")?;

        init_rx
            .recv_timeout(Duration::from_secs(2))
            .context("hotkey thread init timeout (2s)")?
            .context("hotkey thread init error")?;

        log::info!(
            "hotkey installed: trigger={:?}, mode={:?}",
            config.trigger,
            config.mode
        );

        Ok(Self {
            events: rx,
            _thread: thread,
        })
    }

    pub async fn recv(&mut self) -> Option<HotkeyEvent> {
        self.events.recv().await
    }
}

struct HookState {
    trigger_vk: u32,
    mode: Mode,
    /// Mirrors the physical key state so we can filter out auto-repeats
    /// (low-level hook has no repeat flag like `WM_KEYDOWN`'s lParam bit 30).
    is_pressed: AtomicBool,
    /// Logical session state. Guarded by a Mutex because we store an
    /// `Instant` inside Hold — not atomically representable. Contention
    /// is effectively zero (only the hook thread ever locks it).
    logical: Mutex<LogicalState>,
    tx: mpsc::UnboundedSender<HotkeyEvent>,
}

#[derive(Debug)]
enum LogicalState {
    Idle,
    /// Recording, key physically held.
    Hold {
        down_at: Instant,
    },
    /// Recording, key was released as a short tap — waiting for the next
    /// key-down to stop. Reached only in [`Mode::Combo`] and [`Mode::Tap`].
    Toggle,
}

static HOOK_STATE: OnceLock<HookState> = OnceLock::new();

fn run_hook_thread(init_tx: std::sync::mpsc::Sender<Result<()>>) {
    unsafe {
        let h_module = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::Error::from(e).context("GetModuleHandleW")));
                return;
            }
        };

        let hook: HHOOK = match SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_kbd_proc),
            Some(h_module.into()),
            0,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = init_tx.send(Err(
                    anyhow::Error::from(e).context("SetWindowsHookExW(WH_KEYBOARD_LL)")
                ));
                return;
            }
        };

        let _ = init_tx.send(Ok(()));

        // Message pump. The low-level hook is called from here; blocking
        // or taking too long causes Windows to silently unhook us.
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Not strictly necessary (OS cleans up on process exit) but tidy.
        let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
    }
}

extern "system" fn low_level_kbd_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    // Negative codes must be passed through per MSDN.
    if n_code < 0 {
        return unsafe { CallNextHookEx(None, n_code, w_param, l_param) };
    }

    let state = match HOOK_STATE.get() {
        Some(s) => s,
        // Hook fired before OnceLock set (shouldn't happen — we install
        // the hook after set) or after shutdown. Pass through.
        None => return unsafe { CallNextHookEx(None, n_code, w_param, l_param) },
    };

    // Safety: Windows guarantees lParam points to a valid KBDLLHOOKSTRUCT
    // for the duration of this callback when nCode >= 0.
    let kb = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
    if kb.vkCode != state.trigger_vk {
        return unsafe { CallNextHookEx(None, n_code, w_param, l_param) };
    }

    match w_param.0 as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => handle_key_down(state),
        WM_KEYUP | WM_SYSKEYUP => handle_key_up(state),
        _ => {}
    }

    // Swallow the event so the focused window never sees our trigger key.
    // This is what stops single Alt taps from activating the window's
    // menu bar (F10 behavior).
    LRESULT(1)
}

fn handle_key_down(state: &HookState) {
    let was_pressed = state.is_pressed.swap(true, Ordering::AcqRel);
    if was_pressed {
        return; // auto-repeat; ignore
    }
    let mut logical = match state.logical.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match state.mode {
        Mode::Combo | Mode::Hold => match *logical {
            LogicalState::Idle => {
                *logical = LogicalState::Hold {
                    down_at: Instant::now(),
                };
                let _ = state.tx.send(HotkeyEvent::Press);
            }
            LogicalState::Toggle => {
                // Combo only: a second tap ends the toggled session.
                *logical = LogicalState::Idle;
                let _ = state.tx.send(HotkeyEvent::Release);
            }
            LogicalState::Hold { .. } => {
                // Would mean we missed a key-up. Shouldn't happen; ignore.
            }
        },
        Mode::Tap => match *logical {
            LogicalState::Idle => {
                *logical = LogicalState::Toggle;
                let _ = state.tx.send(HotkeyEvent::Press);
            }
            LogicalState::Toggle => {
                *logical = LogicalState::Idle;
                let _ = state.tx.send(HotkeyEvent::Release);
            }
            LogicalState::Hold { .. } => { /* unreachable in Tap */ }
        },
    }
}

fn handle_key_up(state: &HookState) {
    let was_pressed = state.is_pressed.swap(false, Ordering::AcqRel);
    if !was_pressed {
        return;
    }
    let mut logical = match state.logical.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match state.mode {
        Mode::Combo => match *logical {
            LogicalState::Hold { down_at } => {
                let elapsed = down_at.elapsed().as_millis();
                if elapsed >= COMBO_HOLD_MS {
                    // Long hold — classical push-to-talk release.
                    *logical = LogicalState::Idle;
                    let _ = state.tx.send(HotkeyEvent::Release);
                } else {
                    // Short tap — keep recording; next tap will stop.
                    *logical = LogicalState::Toggle;
                }
            }
            _ => {}
        },
        Mode::Hold => {
            if matches!(*logical, LogicalState::Hold { .. }) {
                *logical = LogicalState::Idle;
                let _ = state.tx.send(HotkeyEvent::Release);
            }
        }
        Mode::Tap => {
            // Key-up is a no-op; only the next key-down toggles off.
        }
    }
}
