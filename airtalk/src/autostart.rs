//! Autostart via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
//!
//! Registry state is the single source of truth — the installer can
//! pre-seed the `airtalk` value, and both the tray menu and the
//! Settings window read/write the same key at runtime. No duplicate
//! flag in config.toml.

#![cfg(windows)]

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Value name under the Run key. Must match what the Inno Setup script
/// writes, otherwise toggle-from-UI and install-time autostart would
/// target different entries.
const VALUE_NAME: &str = "airtalk";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn success(rc: WIN32_ERROR) -> bool {
    rc == ERROR_SUCCESS
}

/// Read the current autostart command string, if any.
fn read_value() -> Option<String> {
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = wide(RUN_KEY);
        if !success(RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )) {
            return None;
        }

        let value_name = wide(VALUE_NAME);
        // First pass: probe the size of the value.
        let mut size: u32 = 0;
        let rc = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        );
        if !success(rc) || size == 0 {
            let _ = RegCloseKey(hkey);
            return None;
        }

        // Round up to whole u16s (size is bytes).
        let word_count = size.div_ceil(2) as usize;
        let mut buf = vec![0u16; word_count];
        let mut actual = (buf.len() * 2) as u32;
        let rc = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut actual),
        );
        let _ = RegCloseKey(hkey);
        if !success(rc) {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf16_lossy(&buf))
    }
}

/// True if HKCU\...\Run\airtalk exists (any non-empty value).
pub fn is_enabled() -> bool {
    read_value().is_some()
}

/// Register the current executable for autostart. Overwrites any
/// previous value (which may have pointed at an older install path).
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    // Quote the path so spaces survive CreateProcess parsing.
    let quoted = format!("\"{}\"", exe.display());
    write_value(&quoted)
}

/// Remove the autostart entry. Missing value is treated as success —
/// toggling off when not registered is a no-op.
pub fn disable() -> Result<()> {
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = wide(RUN_KEY);
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_WRITE,
            &mut hkey,
        );
        if !success(rc) {
            bail!("RegOpenKeyExW(Run, KEY_WRITE) failed: {:#x}", rc.0);
        }
        let value_name = wide(VALUE_NAME);
        let rc = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
        let _ = RegCloseKey(hkey);
        if !success(rc) && rc != ERROR_FILE_NOT_FOUND {
            bail!("RegDeleteValueW failed: {:#x}", rc.0);
        }
        Ok(())
    }
}

/// Enable/disable in one call. Convenient for checkbox wiring.
pub fn set(enabled: bool) -> Result<()> {
    if enabled {
        enable()
    } else {
        disable()
    }
}

fn write_value(data: &str) -> Result<()> {
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = wide(RUN_KEY);
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_WRITE,
            &mut hkey,
        );
        if !success(rc) {
            bail!("RegOpenKeyExW(Run, KEY_WRITE) failed: {:#x}", rc.0);
        }
        let value_name = wide(VALUE_NAME);
        let wide_data = wide(data);
        let bytes = std::slice::from_raw_parts(
            wide_data.as_ptr() as *const u8,
            wide_data.len() * 2,
        );
        let rc = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            Some(0),
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(hkey);
        if !success(rc) {
            bail!("RegSetValueExW failed: {:#x}", rc.0);
        }
        Ok(())
    }
}
