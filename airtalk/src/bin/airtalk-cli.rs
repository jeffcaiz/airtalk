//! Thin console-subsystem launcher for `airtalk.exe`.
//!
//! `airtalk.exe` is linked with `windows_subsystem = "windows"`, so the
//! shell detaches from it the instant it starts and console-control
//! events (Ctrl+C) don't route cleanly. This launcher is a normal
//! console process. It:
//!
//! 1. Locates `airtalk.exe` next to itself.
//! 2. Spawns it with whatever args we were given (forwarded verbatim).
//! 3. Stays alive while `airtalk.exe` runs so the terminal blocks.
//! 4. Swallows Ctrl+C in *our* handler so the console event still fires
//!    for the whole group (airtalk.exe receives it via its own
//!    `tokio::signal::ctrl_c` path and shuts down gracefully), but the
//!    launcher doesn't exit out from under it. When the child exits,
//!    we propagate its exit code.
//!
//! This file has no `windows_subsystem` attribute — cargo defaults
//! binaries under `src/bin/` to the console subsystem on Windows.
//!
//! The launcher has no version coupling with `airtalk.exe` beyond
//! "they live in the same directory", so you rarely have to rebuild it.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    // 1. Find airtalk.exe next to the launcher.
    let mut target = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("airtalk-cli: current_exe failed: {e}");
            return ExitCode::from(2);
        }
    };
    target.pop();
    target.push(if cfg!(windows) { "airtalk.exe" } else { "airtalk" });

    if !target.exists() {
        eprintln!(
            "airtalk-cli: airtalk binary not found at {}",
            target.display()
        );
        return ExitCode::from(2);
    }

    // 2. Install a Ctrl+C swallower so *we* don't die before the child.
    //    The child is in the same console group and receives
    //    CTRL_C_EVENT independently — it'll run its own shutdown path
    //    and exit, after which our `status()` call returns.
    #[cfg(windows)]
    install_ctrl_c_swallower();

    // 3. Forward args and wait for the child to exit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let status = match Command::new(&target).args(&args).status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("airtalk-cli: failed to launch {}: {e}", target.display());
            return ExitCode::from(2);
        }
    };

    match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(n) => ExitCode::from(n.clamp(1, 255) as u8),
        None => ExitCode::FAILURE, // killed by signal
    }
}

#[cfg(windows)]
fn install_ctrl_c_swallower() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    // Returning TRUE tells Windows "this event is handled, don't invoke
    // the next handler in the chain (including the default one that
    // would terminate us)." The child process gets the event via the
    // console group regardless of what we return.
    unsafe extern "system" fn swallow(_ctrl_type: u32) -> BOOL {
        BOOL(1)
    }

    unsafe {
        let _ = SetConsoleCtrlHandler(Some(Some(swallow)), true);
    }
}
