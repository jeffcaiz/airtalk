//! Deliver transcribed text into the focused window.
//!
//! Two strategies (per DESIGN.md §7):
//!
//!   * [`Strategy::Clipboard`] (default) — back up the current clipboard
//!     text, write ours, `SendInput(Ctrl+V)`, sleep briefly so the target
//!     app actually reads the clipboard, then restore the backup.
//!     Works in essentially every text field, including Office, Chrome,
//!     VSCode, Slack, terminals on Win11.
//!
//!   * [`Strategy::SendInput`] — type the text one UTF-16 code unit at
//!     a time via `KEYEVENTF_UNICODE`. Slower (20–50 ms/char) but
//!     sidesteps UWP / elevated / DRM apps that filter Ctrl+V.
//!
//! The clipboard backup only covers `CF_UNICODETEXT`. Images, files,
//! and custom formats on the clipboard before the paste **will be lost** —
//! this is the same tradeoff macOS voice tools make. A full multi-format
//! backup is tracked as future work.
//!
//! All calls are synchronous. Target total latency is ~150 ms
//! (50 ms grace for the target app to read + 50 ms restore delay).
//! The UI caller is expected to run `paste` inside `spawn_blocking`
//! so the overlay fade doesn't stall on `SendInput`.

#![cfg(windows)]

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_V,
};

// ─── Public API ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Set clipboard → `Ctrl+V` → restore. Default.
    Clipboard,
    /// Type characters directly with `KEYEVENTF_UNICODE`. Use for apps
    /// that block `Ctrl+V` (UWP, some elevated processes).
    #[allow(dead_code)] // exposed through settings once that module lands
    SendInput,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub strategy: Strategy,
    /// How long to wait between `SetClipboardData` + `Ctrl+V` and the
    /// restore step. Must be long enough for the target app to have
    /// actually consumed the clipboard.
    pub paste_delay: Duration,
    /// Max attempts to grab the clipboard before giving up. Clipboard
    /// viewers / automation tools occasionally hold it briefly.
    pub open_attempts: u32,
    /// Sleep between clipboard-open retries.
    pub open_retry: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            strategy: Strategy::Clipboard,
            paste_delay: Duration::from_millis(80),
            open_attempts: 12,
            open_retry: Duration::from_millis(12),
        }
    }
}

pub fn paste(text: &str, config: &Config) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    match config.strategy {
        Strategy::Clipboard => paste_via_clipboard(text, config),
        Strategy::SendInput => paste_via_sendinput(text),
    }
}

// ─── Clipboard strategy ────────────────────────────────────────────────

fn paste_via_clipboard(text: &str, config: &Config) -> Result<()> {
    let backup = read_clipboard_unicode(config).ok().flatten();
    write_clipboard_unicode(text, config).context("write our text to clipboard")?;
    // Do the paste but keep the error, if any. We restore the backup
    // either way — we don't want our transcript sitting on the user's
    // clipboard uninvited. The recovery window exposes an explicit
    // "copy" button for the fail path so that stays user-controlled.
    let paste_result = send_ctrl_v().context("SendInput(Ctrl+V)");
    sleep(config.paste_delay);
    restore_clipboard(backup.as_deref(), config);
    paste_result
}

fn restore_clipboard(backup: Option<&str>, config: &Config) {
    match backup {
        Some(text) => {
            if let Err(e) = write_clipboard_unicode(text, config) {
                log::warn!("failed to restore prior clipboard: {e}");
            }
        }
        None => {
            if let Err(e) = clear_clipboard(config) {
                log::warn!("failed to clear clipboard after paste: {e}");
            }
        }
    }
}

/// Public helper for the recovery flow's "Copy to clipboard" button.
/// Writes `text` as CF_UNICODETEXT, overwriting whatever was there.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    write_clipboard_unicode(text, &Config::default())
}

fn open_clipboard(config: &Config) -> Result<()> {
    for _ in 0..config.open_attempts {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(());
        }
        sleep(config.open_retry);
    }
    bail!(
        "could not open clipboard after {} attempts",
        config.open_attempts
    )
}

fn read_clipboard_unicode(config: &Config) -> Result<Option<String>> {
    open_clipboard(config)?;
    let result = unsafe {
        match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(h) => {
                let hglobal = HGLOBAL(h.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if ptr.is_null() {
                    Ok(None)
                } else {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                        // Safety cap: clipboard text should never be 16M+ u16s.
                        if len > 16 * 1024 * 1024 {
                            break;
                        }
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    let s = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(hglobal);
                    Ok(Some(s))
                }
            }
            Err(_) => Ok(None),
        }
    };
    let _ = unsafe { CloseClipboard() };
    result
}

fn write_clipboard_unicode(text: &str, config: &Config) -> Result<()> {
    // UTF-16 + null terminator.
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes = utf16.len() * std::mem::size_of::<u16>();

    open_clipboard(config)?;
    let outcome = unsafe {
        let _ = EmptyClipboard();
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes).context("GlobalAlloc")?;
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            bail!("GlobalLock returned null");
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        let _ = GlobalUnlock(hmem);
        // On success, the system takes ownership of the HGLOBAL — we
        // must NOT free it.
        SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0)))
            .context("SetClipboardData")?;
        Ok::<(), anyhow::Error>(())
    };
    let _ = unsafe { CloseClipboard() };
    outcome
}

fn clear_clipboard(config: &Config) -> Result<()> {
    open_clipboard(config)?;
    unsafe {
        let _ = EmptyClipboard();
        let _ = CloseClipboard();
    }
    Ok(())
}

// ─── SendInput ─────────────────────────────────────────────────────────

fn send_ctrl_v() -> Result<()> {
    let inputs = [
        key_event(VK_CONTROL, false),
        key_event(VK_V, false),
        key_event(VK_V, true),
        key_event(VK_CONTROL, true),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(anyhow!(
            "SendInput injected {sent}/{} events — UIPI / protected mode may be blocking us",
            inputs.len()
        ));
    }
    Ok(())
}

fn paste_via_sendinput(text: &str) -> Result<()> {
    // Two INPUTs per UTF-16 code unit (key-down + key-up). Surrogate pairs
    // are naturally handled because we iterate on code units, not chars.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let mut inputs: Vec<INPUT> = Vec::with_capacity(utf16.len() * 2);
    for &unit in &utf16 {
        inputs.push(unicode_event(unit, false));
        inputs.push(unicode_event(unit, true));
    }
    // Chunk to avoid delivering one massive batch to the input queue — some
    // apps drop events if too many arrive in a single SendInput call.
    const CHUNK: usize = 64;
    for chunk in inputs.chunks(CHUNK) {
        let sent = unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        if (sent as usize) != chunk.len() {
            bail!("SendInput injected {sent}/{} events", chunk.len());
        }
    }
    Ok(())
}

fn key_event(vk: VIRTUAL_KEY, is_up: bool) -> INPUT {
    let flags = if is_up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_event(code_unit: u16, is_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if is_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code_unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
