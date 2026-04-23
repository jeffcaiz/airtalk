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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_CAPITAL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Combo,
    Hold,
    Tap,
}

const MODE_COMBO: u8 = 0;
const MODE_HOLD: u8 = 1;
const MODE_TAP: u8 = 2;

fn encode_mode(m: Mode) -> u8 {
    match m {
        Mode::Combo => MODE_COMBO,
        Mode::Hold => MODE_HOLD,
        Mode::Tap => MODE_TAP,
    }
}

fn decode_mode(n: u8) -> Mode {
    match n {
        MODE_HOLD => Mode::Hold,
        MODE_TAP => Mode::Tap,
        _ => Mode::Combo,
    }
}

/// Threshold (ms) below which a key-up in [`Mode::Combo`] is treated as
/// a short tap (keep recording, wait for next tap to stop) rather than
/// a push-to-talk release.
pub const COMBO_HOLD_MS: u128 = 300;

/// Keys we permit as hotkeys. Restricted to modifier-type keys because
/// the low-level hook swallows the trigger — a bare character key would
/// be unusable for normal typing afterwards. F1–F24 were considered but
/// left out for ergonomics (not convenient to press one-handed).
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

fn trigger_vk(t: Trigger) -> u32 {
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
    };
    vk as u32
}

pub struct Hotkey {
    events: mpsc::UnboundedReceiver<HotkeyEvent>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hotkey {
    pub fn start(config: Config) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = HookState {
            trigger_vk: AtomicU32::new(trigger_vk(config.trigger)),
            mode: AtomicU8::new(encode_mode(config.mode)),
            is_pressed: AtomicBool::new(false),
            logical: Mutex::new(LogicalState::Idle),
            tx,
        };
        HOOK_STATE
            .set(state)
            .map_err(|_| anyhow!("Hotkey::start already called in this process"))?;

        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32>>();
        let thread = std::thread::Builder::new()
            .name("airtalk-hotkey".into())
            .spawn(move || {
                run_hook_thread(init_tx);
            })
            .context("spawn hotkey thread")?;

        let thread_id = init_rx
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
            thread_id,
            thread: Some(thread),
        })
    }

    pub async fn recv(&mut self) -> Option<HotkeyEvent> {
        self.events.recv().await
    }

    /// Swap trigger / mode live. If a recording session is currently
    /// active (user pressed the old trigger and hasn't released), we
    /// synthesize a `Release` so the main loop ends that session
    /// cleanly — otherwise it would hang waiting for a key-up on a key
    /// the hook no longer watches.
    pub fn reconfigure(&self, config: Config) -> Result<()> {
        let state = HOOK_STATE
            .get()
            .context("Hotkey::reconfigure called before Hotkey::start")?;
        let was_recording = {
            let mut logical = state
                .logical
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let was = matches!(*logical, LogicalState::Hold { .. } | LogicalState::Toggle);
            *logical = LogicalState::Idle;
            was
        };
        state.is_pressed.store(false, Ordering::Release);
        state
            .trigger_vk
            .store(trigger_vk(config.trigger), Ordering::Release);
        state
            .mode
            .store(encode_mode(config.mode), Ordering::Release);
        if was_recording {
            let _ = state.tx.send(HotkeyEvent::Release);
        }
        log::info!(
            "hotkey reconfigured: trigger={:?}, mode={:?}",
            config.trigger,
            config.mode
        );
        Ok(())
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HookState {
    /// Atomic because `Hotkey::reconfigure` swaps this live from the
    /// main loop while the hook thread is reading on every keystroke.
    trigger_vk: AtomicU32,
    /// Encoded via `encode_mode` / `decode_mode` — u8 because atomics
    /// over enum aren't built in.
    mode: AtomicU8,
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

fn run_hook_thread(init_tx: std::sync::mpsc::Sender<Result<u32>>) {
    unsafe {
        let thread_id = GetCurrentThreadId();
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

        let _ = init_tx.send(Ok(thread_id));

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
    if kb.vkCode != state.trigger_vk.load(Ordering::Acquire) {
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
    let mode = decode_mode(state.mode.load(Ordering::Acquire));
    match mode {
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
    let mode = decode_mode(state.mode.load(Ordering::Acquire));
    match mode {
        Mode::Combo => {
            if let LogicalState::Hold { down_at } = *logical {
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
        }
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
