//! WASAPI device-change watcher.
//!
//! Registers an `IMMNotificationClient` on a dedicated MTA thread and
//! feeds a [`ControlMsg::Reevaluate`] into the audio capture thread on
//! any event that could change *which device we should be on*:
//!
//!   * `OnDefaultDeviceChanged(eCapture, eConsole, _)` — the classic
//!     Bluetooth-headset-connected-after-launch case: the OS default
//!     shifts to the new device and `DeviceChoice::Auto` should follow.
//!   * `OnDeviceAdded` / `OnDeviceRemoved` / `OnDeviceStateChanged` —
//!     lets a `DeviceChoice::Named(x)` recover when `x` gets plugged
//!     back in (or back out), without the user having to touch the tray.
//!
//! Callbacks land on arbitrary COM threadpool threads, so the notifier
//! keeps its `Sender` behind a `Mutex` (`Sender<T>` is `Send` but not
//! `Sync`). We do nothing else on the callback — just bounce to the
//! audio thread and return; audio thread decides if a rebuild is
//! actually warranted.
//!
//! `OnPropertyValueChanged` fires for things like level-meter ticks
//! and is deliberately ignored — acting on it would rebuild streams
//! continuously.

#![cfg(windows)]

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::{Result as WinResult, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows_implement::implement;

use crate::audio::ControlMsg;

/// Handle to the watcher thread. Drop to stop.
pub struct DeviceWatcher {
    shutdown_tx: Sender<()>,
    _thread: JoinHandle<()>,
}

impl DeviceWatcher {
    pub fn start(control_tx: Sender<ControlMsg>) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("airtalk-audio-watch".into())
            .spawn(move || {
                if let Err(e) = run_watch_thread(control_tx, shutdown_rx, &init_tx) {
                    let _ = init_tx.send(Err(e));
                }
            })
            .context("spawn device watcher thread")?;
        init_rx
            .recv_timeout(Duration::from_secs(2))
            .context("watcher init timeout (2 s)")?
            .context("watcher init error")?;
        Ok(Self {
            shutdown_tx,
            _thread: thread,
        })
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        // Wake the parked thread so it can Unregister + CoUninitialize
        // cleanly. We don't block on the join — if the COM call hangs
        // we'd rather let the process exit than deadlock shutdown.
        let _ = self.shutdown_tx.send(());
    }
}

fn run_watch_thread(
    control_tx: Sender<ControlMsg>,
    shutdown_rx: Receiver<()>,
    init_tx: &Sender<Result<()>>,
) -> Result<()> {
    // MTA matches IMMNotificationClient's threadpool delivery model; an
    // STA would require a message pump here just to service callbacks.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            anyhow::bail!("CoInitializeEx: {hr:?}");
        }
    }

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("CoCreateInstance(MMDeviceEnumerator)")?;

    let notifier: IMMNotificationClient = Notifier {
        control_tx: Mutex::new(control_tx),
    }
    .into();
    unsafe { enumerator.RegisterEndpointNotificationCallback(&notifier) }
        .context("RegisterEndpointNotificationCallback")?;
    log::info!("audio-watch: IMMNotificationClient registered");

    let _ = init_tx.send(Ok(()));

    let _ = shutdown_rx.recv();

    unsafe {
        let _ = enumerator.UnregisterEndpointNotificationCallback(&notifier);
        // Drop references *before* CoUninitialize so the COM runtime is
        // still alive to service the final Release calls.
        drop(notifier);
        drop(enumerator);
        CoUninitialize();
    }
    log::info!("audio-watch: unregistered, thread exiting");
    Ok(())
}

#[implement(IMMNotificationClient)]
struct Notifier {
    control_tx: Mutex<Sender<ControlMsg>>,
}

impl Notifier_Impl {
    fn notify(&self) {
        if let Ok(tx) = self.control_tx.lock() {
            let _ = tx.send(ControlMsg::Reevaluate);
        }
    }
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> WinResult<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> WinResult<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> WinResult<()> {
        self.notify();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _pwstrdefault: &PCWSTR,
    ) -> WinResult<()> {
        // Only the console-role capture default. eCommunications fires
        // in parallel for the same user action; acting on both would
        // rebuild twice. eRender is irrelevant to us.
        if flow == eCapture && role == eConsole {
            self.notify();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _pwstrdeviceid: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        // Intentionally ignored — level meter / endpoint property churn
        // would otherwise cause continuous rebuilds.
        Ok(())
    }
}
