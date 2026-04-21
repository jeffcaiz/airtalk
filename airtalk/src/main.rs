//! airtalk UI (native Windows).
//!
//! Default launch: silent background app — tray icon, global hotkey
//! (Right Alt, combo mode), floating overlay, auto-paste on transcribe.
//! A second instance is refused via a named mutex and informed via a
//! message box.
//!
//! Developer-only flags:
//!
//!   * `--smoke-test`  — spawn core, verify Ready handshake, exit.
//!   * `--dev-mic [--seconds N]` — timed mic roundtrip (no hotkey).
//!
//! If started from a terminal (cmd / bash / powershell), logs are
//! attached to the parent console. Otherwise they go to
//! `%APPDATA%\airtalk\logs\ui.log` (truncated each launch).
//!
//! See DESIGN.md §7 / §9 for the full architecture.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod core_client;
mod device_pref;
mod hotkey;
mod overlay;
mod paste;
mod paths;
mod recovery;
#[cfg(windows)]
mod single_instance;
mod tray;

use anyhow::Result;
use std::time::{Duration, Instant};

use airtalk_proto::Response;
use audio::AudioCapture;
use core_client::{CoreClient, SpawnConfig};
use hotkey::{Hotkey, HotkeyEvent};
use overlay::{Overlay, OverlayState};
use recovery::{Recovery, RecoveryEvent};
use tray::{Tray, TrayEvent};

#[cfg(windows)]
const SINGLE_INSTANCE_MUTEX: &str = "Local\\airtalk-singleton-v1";

struct Args {
    smoke_test: bool,
    dev_mic: bool,
    seconds: u64,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    let smoke_test = raw.iter().any(|a| a == "--smoke-test");
    let dev_mic = raw.iter().any(|a| a == "--dev-mic");
    let seconds = raw
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| raw.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    Args {
        smoke_test,
        dev_mic,
        seconds,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Opt into Per-Monitor DPI Aware v2 before any window is created.
    //    Without this the tray menu font looks bitmap-stretched on HiDPI
    //    and our overlay renders at ~half size on 200 %-scaled displays.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 2. Route logs before anything else that might log.
    init_logging();

    // 3. Single-instance — refuse + notify the user if there's already one.
    #[cfg(windows)]
    let _single_guard = {
        match single_instance::SingleInstance::acquire(SINGLE_INSTANCE_MUTEX) {
            Ok(guard) => guard,
            Err(single_instance::AcquireError::AlreadyRunning(_)) => {
                show_info(
                    "airtalk 已经在运行。\n\n请到任务栏右下角找到托盘图标使用。",
                );
                return Ok(());
            }
            Err(single_instance::AcquireError::CreateFailed(e)) => {
                log::warn!("could not create single-instance mutex: {e}; proceeding anyway");
                // Fall through without a guard — worst case two instances.
                return run(&parse_args()).await;
            }
        }
    };

    // Everything below can fail in user-observable ways (core binary
    // missing, overlay init, hotkey hook denied by antivirus, etc.).
    // Surface any early exit via a message box so silent launch failure
    // doesn't leave the user wondering why nothing happened.
    if let Err(e) = run(&parse_args()).await {
        log::error!("airtalk fatal: {e:?}");
        #[cfg(windows)]
        show_fatal(&format!("airtalk 启动失败：\n\n{e}"));
    }
    Ok(())
}

async fn run(args: &Args) -> Result<()> {
    // Spawn core with whatever env keys the user has set. No LLM key →
    // run core with --no-llm so it still boots.
    let mut cfg = SpawnConfig::default_sibling()?;
    let has_llm_key = std::env::var("AIRTALK_LLM_API_KEY").is_ok();
    for var in ["AIRTALK_ASR_API_KEY", "AIRTALK_LLM_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            cfg.env.push((var.into(), v));
        }
    }
    if !has_llm_key {
        cfg.args.push("--no-llm".into());
    }

    let client = CoreClient::spawn(cfg).await?;
    log::info!("handshake complete — core is ready to serve sessions");

    if args.smoke_test {
        tokio::time::sleep(Duration::from_millis(300)).await;
    } else if args.dev_mic {
        run_dev_mic_timed(&client, args.seconds).await?;
    } else {
        run_hotkey_loop(&client).await?;
    }

    client.shutdown().await?;
    log::info!("airtalk UI exiting cleanly");
    Ok(())
}

// ─── Logging: attached console if we have one, else rolling file ──────

fn init_logging() {
    let builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    );
    let mut builder = builder;

    #[cfg(windows)]
    let console_attached = unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS).is_ok()
    };
    #[cfg(not(windows))]
    let console_attached = true;

    if console_attached {
        builder.init();
        return;
    }

    // No parent console — write to file. Truncate each run so the log
    // doesn't grow unbounded. Core's stderr flows here too because
    // core_client pipes it through the same log facade as `[core] …`.
    match open_log_file() {
        Ok(file) => {
            builder
                .target(env_logger::Target::Pipe(Box::new(file)))
                .init();
        }
        Err(_) => {
            // Last-ditch: keep logging to stderr even though nobody reads
            // it. Better than silently dropping on the floor.
            builder.init();
        }
    }
}

fn open_log_file() -> Result<std::fs::File> {
    let path = paths::logs_dir()?.join("ui.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    Ok(file)
}

// ─── Fatal / info dialogs ─────────────────────────────────────────────

#[cfg(windows)]
fn show_info(msg: &str) {
    show_msgbox(msg, false);
}

#[cfg(windows)]
fn show_fatal(msg: &str) {
    show_msgbox(msg, true);
}

#[cfg(windows)]
fn show_msgbox(msg: &str, is_error: bool) {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
    };

    let wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let icon = if is_error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("空·谈 · airtalk"),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

// ─── Modes ────────────────────────────────────────────────────────────

/// Record from the default mic for `seconds` seconds, send to core with
/// VAD on, print the final `Result`.
async fn run_dev_mic_timed(client: &CoreClient, seconds: u64) -> Result<()> {
    let mut audio = AudioCapture::start(device_pref::load())?;
    log::info!("dev-mic: input = \"{}\"", audio.device_name());

    let id = client.begin(true).await?;
    audio.open_gate();
    log::info!("dev-mic: recording for {seconds} s — speak now");

    let end_at = Instant::now() + Duration::from_secs(seconds);
    let mut ended = false;
    let mut level_tick = tokio::time::interval(Duration::from_millis(250));
    level_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            pcm = audio.recv() => {
                match pcm {
                    Some(bytes) => client.chunk(id, bytes).await?,
                    None => anyhow::bail!("audio capture thread died"),
                }
            }
            resp = client.recv() => {
                match resp {
                    Some(Response::Result { text, raw, language, stats, .. }) => {
                        print_result(&text, raw.as_deref(), language.as_deref(), &stats);
                        return Ok(());
                    }
                    Some(Response::Error { message, .. }) => {
                        anyhow::bail!("core error: {message}");
                    }
                    Some(other) => log::warn!("unexpected response: {other:?}"),
                    None => anyhow::bail!("core closed unexpectedly"),
                }
            }
            _ = tokio::time::sleep_until(end_at.into()), if !ended => {
                audio.close_gate();
                let _ = audio.drain_pending();
                client.end(id).await?;
                ended = true;
                log::info!("dev-mic: recording done, awaiting result…");
            }
            _ = level_tick.tick() => {
                let lvl = audio.level();
                let bars = (lvl * 40.0).min(40.0) as usize;
                eprint!("\r  mic level: [{}{}] rms={:.3}     ",
                    "█".repeat(bars), " ".repeat(40 - bars), lvl);
            }
        }
    }
}

/// Default runtime. Tray icon + overlay + hotkey; transcribed text is
/// pasted into the focused window. Exits on tray Quit or Ctrl-C.
async fn run_hotkey_loop(client: &CoreClient) -> Result<()> {
    let initial_mic = device_pref::load();
    let mut audio = AudioCapture::start(initial_mic.clone())?;
    let overlay = Overlay::start(audio.level_source())?;
    let mut hotkey = Hotkey::start(hotkey::Config::default())?;
    let mut tray = Tray::start(initial_mic.clone())?;
    let mut recovery = Recovery::start()?;
    log::info!("mic: \"{}\"", audio.device_name());

    let mut current_session: Option<u64> = None;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            biased;
            res = &mut ctrl_c => {
                res?;
                shutdown_current_session(&mut current_session, &audio, client).await;
                overlay.set_state(OverlayState::Idle);
                log::info!("Ctrl-C — exiting");
                return Ok(());
            }
            tray_ev = tray.recv() => {
                match tray_ev {
                    Some(TrayEvent::Quit) => {
                        shutdown_current_session(&mut current_session, &audio, client).await;
                        overlay.set_state(OverlayState::Idle);
                        log::info!("tray: Quit — exiting");
                        return Ok(());
                    }
                    Some(TrayEvent::OpenSettings) => {
                        // TODO: launch the settings window once that module lands.
                        log::info!("tray: Settings requested (not yet implemented)");
                    }
                    Some(TrayEvent::SelectMicrophone(choice)) => {
                        log::info!("tray: switch mic → {choice:?}");
                        audio.switch_to(choice.clone());
                        if let Err(e) = device_pref::save(&choice) {
                            log::warn!("could not persist mic choice: {e}");
                        }
                        tray.set_current_mic(choice);
                    }
                    None => anyhow::bail!("tray channel closed"),
                }
            }
            pcm = audio.recv(), if current_session.is_some() => {
                match pcm {
                    Some(bytes) => {
                        if let Some(id) = current_session {
                            client.chunk(id, bytes).await?;
                        }
                    }
                    None => anyhow::bail!("audio thread died"),
                }
            }
            rec_ev = recovery.recv() => {
                match rec_ev {
                    Some(RecoveryEvent::Copied) => log::info!("recovery: copied"),
                    Some(RecoveryEvent::Dismissed) => log::info!("recovery: dismissed"),
                    None => { /* channel closed — fine, thread parked */ }
                }
            }
            ev = hotkey.recv() => {
                match ev {
                    Some(HotkeyEvent::Press) => {
                        // Any previous recovery popup is implicitly answered
                        // by the user starting a new session — dismiss it.
                        recovery.hide();
                        if let Some(old) = current_session.take() {
                            log::warn!("Press during session {old} — superseding");
                            audio.close_gate();
                            let _ = audio.drain_pending();
                            let _ = client.cancel(old).await;
                        }
                        let id = client.begin(true).await?;
                        audio.open_gate();
                        current_session = Some(id);
                        overlay.set_state(OverlayState::Recording);
                    }
                    Some(HotkeyEvent::Release) => {
                        if let Some(id) = current_session.take() {
                            audio.close_gate();
                            let _ = audio.drain_pending();
                            client.end(id).await?;
                            overlay.set_state(OverlayState::Processing);
                        }
                    }
                    None => anyhow::bail!("hotkey channel closed"),
                }
            }
            resp = client.recv() => {
                match resp {
                    Some(Response::Result { text, stats, .. }) => {
                        log::info!(
                            "result: pcm_sent={}ms asr={}ms total={}ms text=\"{}\"",
                            stats.pcm_sent_to_asr_ms,
                            stats.asr_latency_ms,
                            stats.total_latency_ms,
                            text,
                        );
                        overlay.set_state(OverlayState::Idle);
                        // Paste on the blocking pool so the select loop keeps
                        // responding. On failure, hand the text off to the
                        // recovery popup so the user can still recover it.
                        let text_to_paste = text.clone();
                        let recovery_handle = recovery.handle();
                        tokio::task::spawn_blocking(move || {
                            match paste::paste(&text_to_paste, &paste::Config::default()) {
                                Ok(_) => {}
                                Err(e) => {
                                    log::warn!("paste failed: {e}");
                                    recovery_handle.show(text_to_paste);
                                }
                            }
                        });
                    }
                    Some(Response::Error { id, message }) => {
                        if message == airtalk_proto::error_code::NO_AUDIO {
                            log::info!("[{id}] no speech detected");
                            overlay.set_state(OverlayState::Idle);
                        } else if message == airtalk_proto::error_code::CANCELLED
                                  || message == airtalk_proto::error_code::SUPERSEDED {
                            // We caused this — don't flash Error at the user.
                        } else {
                            log::error!("[{id}] {message}");
                            overlay.set_state(OverlayState::Error);
                        }
                    }
                    Some(Response::Ready { .. }) => { /* shouldn't re-fire */ }
                    None => anyhow::bail!("core closed unexpectedly"),
                }
            }
        }
    }
}

async fn shutdown_current_session(
    current: &mut Option<u64>,
    audio: &AudioCapture,
    client: &CoreClient,
) {
    if let Some(id) = current.take() {
        audio.close_gate();
        let _ = client.cancel(id).await;
    }
}

fn print_result(
    text: &str,
    raw: Option<&str>,
    language: Option<&str>,
    stats: &airtalk_proto::SessionStats,
) {
    println!();
    println!("    text : {text}");
    if let Some(raw) = raw {
        if raw != text {
            println!("    raw  : {raw}");
        }
    }
    if let Some(lang) = language {
        println!("    lang : {lang}");
    }
    println!(
        "    stats: pcm_sent={}ms asr={}ms total={}ms asr_calls={}",
        stats.pcm_sent_to_asr_ms, stats.asr_latency_ms, stats.total_latency_ms, stats.asr_calls
    );
}
