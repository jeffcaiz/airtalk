//! airtalk UI (native Windows).
//!
//! This crate is a stub. See DESIGN.md §UI for the planned components:
//!
//!   - tray icon (Shell_NotifyIcon)
//!   - global hotkey (WH_KEYBOARD_LL low-level hook, for Alt-tap etc.)
//!   - overlay window (layered + transparent + noactivate + topmost)
//!   - microphone capture (cpal) with resampling to 16 kHz PCM16 mono
//!   - paste (SetClipboardData + SendInput Ctrl+V, with SendInput
//!     unicode-char fallback)
//!   - settings window
//!   - core_client: spawn airtalk-core.exe and exchange framed
//!     Requests/Responses over stdin/stdout; attach child to a
//!     Job Object so it dies when the UI dies.

fn main() -> anyhow::Result<()> {
    env_logger::init();
    log::info!("airtalk UI stub — not yet implemented");
    log::info!("see DESIGN.md §UI and §Implementation status");
    Ok(())
}
