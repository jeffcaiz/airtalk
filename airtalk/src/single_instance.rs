//! Ensure only one airtalk per user session.
//!
//! Uses a named mutex in the `Local\` namespace (per-session, not
//! machine-global) so two different users can each run their own
//! instance but a user can't accidentally launch a second copy and
//! end up with two tray icons + two core processes.

#![cfg(windows)]

use anyhow::{anyhow, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// RAII guard — the mutex is held for as long as this value lives.
/// On Drop the handle is closed, releasing the name for future runs.
pub struct SingleInstance(HANDLE);

/// Returned by [`SingleInstance::acquire`] when another process already
/// holds the name. The caller should tell the user and exit.
#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another airtalk instance is already running")
    }
}

impl std::error::Error for AlreadyRunning {}

impl SingleInstance {
    /// Try to take the mutex. Returns [`AlreadyRunning`] if a prior
    /// instance got there first.
    pub fn acquire(name: &str) -> Result<Self, AcquireError> {
        let full: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let h = CreateMutexW(None, false, PCWSTR(full.as_ptr()))
                .map_err(|e| AcquireError::CreateFailed(anyhow!("CreateMutexW: {e}")))?;
            // NB: we must check GetLastError *before* doing anything else —
            // Windows sets it to ERROR_ALREADY_EXISTS even when CreateMutexW
            // returns a valid handle (it opens the existing mutex in that case).
            let err = GetLastError();
            if err == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return Err(AcquireError::AlreadyRunning(AlreadyRunning));
            }
            Ok(SingleInstance(h))
        }
    }
}

#[derive(Debug)]
pub enum AcquireError {
    AlreadyRunning(AlreadyRunning),
    CreateFailed(anyhow::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(e) => write!(f, "{e}"),
            Self::CreateFailed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
