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
//!   * `--debug` — log at `debug` level and force a foreground console
//!     (allocates one if launched from Explorer / tray / a shortcut).
//!
//! For a smooth dev workflow in cmd / PowerShell, prefer launching via
//! the sibling `airtalk-cli.exe` (see `src/bin/airtalk-cli.rs`). Because
//! this binary is linked with `windows_subsystem = "windows"`, shells
//! detach from it immediately and Ctrl+C doesn't route cleanly — the
//! console-subsystem launcher solves both.
//!
//! If started from a terminal (cmd / bash / powershell), logs are
//! attached to the parent console. Otherwise they go to
//! `%APPDATA%\airtalk\logs\ui.log` (truncated each launch).
//!
//! See DESIGN.md §7 / §9 for the full architecture.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
#[cfg(windows)]
mod autostart;
mod core_client;
mod hotkey;
mod overlay;
mod paste;
mod paths;
mod settings;
#[cfg(windows)]
mod single_instance;
#[cfg(windows)]
mod slint_bridge;
#[cfg(windows)]
mod slint_ui;
mod tray;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use airtalk_proto::Response;
use audio::AudioCapture;
use core_client::CoreClient;
use hotkey::{Hotkey, HotkeyEvent};
use overlay::{Overlay, OverlayState};
use settings::SettingsEvent;
use slint_bridge::{RecoveryEvent, SlintBridge};
use tray::{Tray, TrayEvent};

#[cfg(windows)]
const SINGLE_INSTANCE_MUTEX: &str = "Local\\airtalk-singleton-v1";

struct Args {
    smoke_test: bool,
    dev_mic: bool,
    dev_recovery: bool,
    recovery_text: Option<String>,
    seconds: u64,
    debug: bool,
}

impl Args {
    /// True if any dev-only mode is active. These short-lived modes
    /// bypass the single-instance guard so they can run alongside a
    /// normal airtalk instance during testing.
    fn is_dev_mode(&self) -> bool {
        self.smoke_test || self.dev_mic || self.dev_recovery
    }
}

fn print_help() {
    println!(
        "AirTalk {} — voice input for Windows",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Usage: airtalk [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --debug           Log at debug level, force a foreground console");
    println!("  --smoke-test      Spawn core, verify Ready handshake, exit");
    println!("  --dev-mic         Timed mic roundtrip (developer)");
    println!("  --seconds N       Duration for --dev-mic (default 3)");
    println!("  --dev-recovery    Show the recovery popup with sample text, exit on dismiss");
    println!("  --text <string>   Override the --dev-recovery body text");
    println!("  -V, --version     Print version and exit");
    println!("  -h, --help        Show this help");
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    let smoke_test = raw.iter().any(|a| a == "--smoke-test");
    let dev_mic = raw.iter().any(|a| a == "--dev-mic");
    let dev_recovery = raw.iter().any(|a| a == "--dev-recovery");
    let debug = raw.iter().any(|a| a == "--debug");
    let seconds = raw
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| raw.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let recovery_text = raw
        .iter()
        .position(|a| a == "--text")
        .and_then(|i| raw.get(i + 1))
        .cloned();
    Args {
        smoke_test,
        dev_mic,
        dev_recovery,
        recovery_text,
        seconds,
        debug,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 0. `--version` / `-V` / `--help` / `-h` print and exit. Handled
    //    before anything else so users running from a console don't
    //    get tangled up in single-instance or logging init.
    let raw: Vec<String> = std::env::args().collect();
    if raw.iter().any(|a| a == "--version" || a == "-V") {
        println!("AirTalk {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

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

    // 2. Parse args, then route logs based on --debug.
    let args = parse_args();
    init_logging(args.debug);

    // 3. Single-instance — refuse + notify the user if there's already one.
    //    Dev modes (--smoke-test / --dev-mic / --dev-recovery) bypass
    //    this so they can run alongside a normal airtalk for testing.
    #[cfg(windows)]
    let _single_guard = if args.is_dev_mode() {
        None
    } else {
        match single_instance::SingleInstance::acquire(SINGLE_INSTANCE_MUTEX) {
            Ok(guard) => Some(guard),
            Err(single_instance::AcquireError::AlreadyRunning(_)) => {
                show_info("AirTalk 已经在运行。\n\n请到任务栏右下角找到托盘图标使用。");
                return Ok(());
            }
            Err(single_instance::AcquireError::CreateFailed(e)) => {
                log::warn!("could not create single-instance mutex: {e}; proceeding anyway");
                // Fall through without a guard — worst case two instances.
                None
            }
        }
    };

    // Everything below can fail in user-observable ways (core binary
    // missing, overlay init, hotkey hook denied by antivirus, etc.).
    // Surface any early exit via a message box so silent launch failure
    // doesn't leave the user wondering why nothing happened.
    if let Err(e) = run(&args).await {
        log::error!("airtalk fatal: {e:?}");
        #[cfg(windows)]
        show_fatal(&format!("AirTalk 启动失败：\n\n{e}"));
    }
    Ok(())
}

async fn run(args: &Args) -> Result<()> {
    // Spawn core with whatever env keys the user has set. No LLM key →
    // run core with --no-llm so it still boots.
    log::info!("handshake complete — core is ready to serve sessions");

    if args.smoke_test {
        let client = spawn_core_from_settings().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.shutdown().await?;
    } else if args.dev_mic {
        let client = spawn_core_from_settings().await?;
        run_dev_mic_timed(&client, args.seconds).await?;
        client.shutdown().await?;
    } else if args.dev_recovery {
        run_dev_recovery(args.recovery_text.clone()).await?;
    } else {
        run_hotkey_loop().await?;
    }

    log::info!("airtalk UI exiting cleanly");
    Ok(())
}

// ─── Logging: attached console if we have one, else rolling file ──────

fn init_logging(debug: bool) {
    let default_filter = if debug { "debug" } else { "info" };
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter));
    // Silence a chatty warning Slint's text pipeline emits every time it
    // lays out CJK text: icu_segmenter's ChineseOrJapanese bucket asks
    // for a CJK dictionary that isn't bundled, and icu_provider logs a
    // `log::warn!("No segmentation model for language: ja")` for each
    // text node on every relayout. The fallback (per-codepoint split)
    // is correct for ideographic text, so the warning is pure noise.
    // Enforce unconditionally so it also applies when users set
    // RUST_LOG= to something permissive.
    builder.filter_module("icu_provider", log::LevelFilter::Error);

    #[cfg(windows)]
    let console_attached = unsafe {
        use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
        // Prefer the parent terminal's console if there is one — typical
        // when launched from PowerShell/cmd. If that fails and we're in
        // --debug mode, allocate a fresh console window so the developer
        // can see logs live even when launched via Explorer or tray.
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            true
        } else if debug {
            AllocConsole().is_ok()
        } else {
            false
        }
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
            w!("AirTalk - 空·谈"),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

// ─── Modes ────────────────────────────────────────────────────────────

/// Record from the default mic for `seconds` seconds, send to core with
/// VAD on, print the final `Result`.
async fn run_dev_mic_timed(client: &CoreClient, seconds: u64) -> Result<()> {
    let config = settings::load_snapshot()?;
    let mut audio = AudioCapture::start(
        settings::audio_choice_from_config(&config.config),
        config.config.audio.instant_record,
    )?;
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

/// Dev-only: pop the recovery window with sample text, wait for the
/// user to click 复制 / Dismiss / press Esc, print what happened, exit.
/// Bypasses single-instance so it can be run while a normal airtalk
/// is live — handy for smoke-testing the popup's focus, positioning,
/// topmost behavior, and scroll overflow.
#[cfg(windows)]
async fn run_dev_recovery(custom: Option<String>) -> Result<()> {
    let text = custom.unwrap_or_else(|| {
        // Mix short + long + CJK/Latin + hard line to exercise wrap,
        // scroll overflow, and font handling.
        "我们先试一下啊，混杂的情况，譬如说呃，在香港，setting 界面是很困难的。\n\n\
        接下来是更长的一段测试文本，用来验证 recovery 窗口的滚动行为和 word-wrap: \
        今天下午我们讨论了 React 的 useState hook 在 SSR 场景下的若干坑位，\
        尤其是 useEffect 在 hydration 阶段的执行时机问题。关键是 setState 触发的 \
        re-render 与 Suspense 的边界之间如何协作。\n\n\
        This is a long English paragraph meant to verify word wrap on Latin text \
        at the declared 440px window width, which should be perfectly readable \
        without horizontal scrollbars. If you see horizontal clipping, wrap is broken.\n\n\
        最后一行短文本。"
            .to_string()
    });

    let (slint, mut slint_events) = slint_bridge::SlintBridge::new();
    slint.show_recovery(text);

    log::info!("dev-recovery: waiting for dismiss (× / Dismiss / Esc / 复制)…");
    match slint_events.recovery.recv().await {
        Some(slint_bridge::RecoveryEvent::Copied) => {
            log::info!("dev-recovery: Copied — clipboard now holds the text");
        }
        Some(slint_bridge::RecoveryEvent::Dismissed) => {
            log::info!("dev-recovery: Dismissed");
        }
        None => anyhow::bail!("recovery channel closed without an event"),
    }
    Ok(())
}

/// Default runtime. Tray icon + overlay + hotkey; transcribed text is
/// pasted into the focused window. Exits on tray Quit or Ctrl-C.
async fn run_hotkey_loop() -> Result<()> {
    let initial_snapshot = settings::load_snapshot()?;
    let mut current_mic = settings::audio_choice_from_config(&initial_snapshot.config);
    let mut current_hotkey = settings::hotkey_config_from_config(&initial_snapshot.config);
    let mut current_instant_record = initial_snapshot.config.audio.instant_record;
    let mut audio = AudioCapture::start(current_mic.clone(), current_instant_record)?;
    let overlay = Overlay::start(audio.level_source())?;
    let mut hotkey = Hotkey::start(current_hotkey)?;
    let mut tray = Tray::start(current_mic.clone())?;
    let (slint, mut slint_events) = SlintBridge::new();
    log::info!("mic: \"{}\"", audio.device_name());

    let mut client = match spawn_core_from_settings().await {
        Ok(client) => Some(client),
        Err(e) => {
            log::warn!("core unavailable at startup: {e:#}");
            slint.open_settings();
            None
        }
    };

    let mut current_session: Option<u64> = None;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            biased;
            res = &mut ctrl_c => {
                res?;
                shutdown_current_session(&mut current_session, &audio, client.as_ref()).await;
                overlay.set_state(OverlayState::Idle);
                log::info!("Ctrl-C — exiting");
                if let Some(client) = client.take() {
                    client.shutdown().await?;
                }
                return Ok(());
            }
            tray_ev = tray.recv() => {
                match tray_ev {
                    Some(TrayEvent::Quit) => {
                        shutdown_current_session(&mut current_session, &audio, client.as_ref()).await;
                        overlay.set_state(OverlayState::Idle);
                        log::info!("tray: Quit — exiting");
                        if let Some(client) = client.take() {
                            client.shutdown().await?;
                        }
                        return Ok(());
                    }
                    Some(TrayEvent::OpenSettings) => {
                        shutdown_current_session(&mut current_session, &audio, client.as_ref()).await;
                        overlay.set_state(OverlayState::Idle);
                        log::info!("tray: Settings requested");
                        slint.open_settings();
                    }
                    Some(TrayEvent::SelectMicrophone(choice)) => {
                        log::info!("tray: switch mic → {choice:?}");
                        audio.switch_to(choice.clone());
                        if let Err(e) = settings::save_audio_choice(&choice) {
                            log::warn!("could not persist mic choice: {e}");
                        }
                        current_mic = choice.clone();
                        tray.set_current_mic(choice);
                    }
                    Some(TrayEvent::SetAutostart(enabled)) => {
                        log::info!("tray: autostart → {enabled}");
                        #[cfg(windows)]
                        if let Err(e) = autostart::set(enabled) {
                            log::warn!("autostart toggle failed: {e:#}");
                        }
                    }
                    None => anyhow::bail!("tray channel closed"),
                }
            }
            pcm = audio.recv(), if current_session.is_some() => {
                match pcm {
                    Some(bytes) => {
                        if let Some(id) = current_session {
                            if let Some(client) = client.as_ref() {
                                client.chunk(id, bytes).await?;
                            }
                        }
                    }
                    None => anyhow::bail!("audio thread died"),
                }
            }
            rec_ev = slint_events.recovery.recv() => {
                match rec_ev {
                    Some(RecoveryEvent::Copied) => log::info!("recovery: copied"),
                    Some(RecoveryEvent::Dismissed) => log::info!("recovery: dismissed"),
                    None => { /* channel closed — fine, thread parked */ }
                }
            }
            ev = hotkey.recv() => {
                match ev {
                    Some(HotkeyEvent::Press) => {
                        if client.is_none() {
                            overlay.set_state(OverlayState::Error);
                            slint.open_settings();
                            continue;
                        }
                        // Any previous recovery popup is implicitly answered
                        // by the user starting a new session — dismiss it.
                        slint.hide_recovery();
                        if let Some(old) = current_session.take() {
                            log::warn!("Press during session {old} — superseding");
                            audio.close_gate();
                            let _ = audio.drain_pending();
                            if let Some(client) = client.as_ref() {
                                let _ = client.cancel(old).await;
                            }
                        }
                        let id = client
                            .as_ref()
                            .context("core unavailable")?
                            .begin(true)
                            .await?;
                        audio.open_gate();
                        current_session = Some(id);
                        overlay.set_state(OverlayState::Recording);
                    }
                    Some(HotkeyEvent::Release) => {
                        if let Some(id) = current_session.take() {
                            audio.close_gate();
                            let _ = audio.drain_pending();
                            if let Some(client) = client.as_ref() {
                                client.end(id).await?;
                            }
                            overlay.set_state(OverlayState::Processing);
                        }
                    }
                    None => anyhow::bail!("hotkey channel closed"),
                }
            }
            resp = recv_core(&client), if client.is_some() => {
                match resp {
                    Some(Response::Result { text, stats, .. }) => {
                        log::info!(
                            "result: pcm_sent={}ms asr={}ms total={}ms text=\"{}\"",
                            stats.pcm_sent_to_asr_ms,
                            stats.asr_latency_ms,
                            stats.total_latency_ms,
                            text,
                        );
                        // Keep Processing visible through the paste — we flip
                        // to Success (or Idle, on failure) from inside the
                        // blocking task so the user gets a visual confirmation
                        // that text actually landed. Without this, the overlay
                        // just disappears when the result arrives and paste is
                        // silent.
                        let text_to_paste = text.clone();
                        let recovery_handle = slint.recovery_handle();
                        let overlay_handle = overlay.handle();
                        tokio::task::spawn_blocking(move || {
                            match paste::paste(&text_to_paste, &paste::Config::default()) {
                                Ok(_) => {
                                    overlay_handle.set_state(OverlayState::Success);
                                }
                                Err(e) => {
                                    log::warn!("paste failed: {e}");
                                    overlay_handle.set_state(OverlayState::Idle);
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
            settings_ev = slint_events.settings.recv() => {
                match settings_ev {
                    Some(SettingsEvent::Applied(snapshot)) => {
                        let desired_mic = settings::audio_choice_from_config(&snapshot.config);
                        let desired_instant_record = snapshot.config.audio.instant_record;
                        // instant_record is a constructor parameter of
                        // AudioCapture, so a change means a full rebuild.
                        // We share the level Arc with Overlay, so reuse
                        // it; otherwise the overlay waveform goes flat.
                        if desired_instant_record != current_instant_record {
                            let level = audio.level_source();
                            audio = AudioCapture::restart_with_level(
                                desired_mic.clone(),
                                desired_instant_record,
                                level,
                            )?;
                            current_mic = desired_mic.clone();
                            current_instant_record = desired_instant_record;
                            tray.set_current_mic(desired_mic);
                        } else if desired_mic != current_mic {
                            audio.switch_to(desired_mic.clone());
                            tray.set_current_mic(desired_mic.clone());
                            current_mic = desired_mic;
                        }
                        let desired_hotkey = settings::hotkey_config_from_config(&snapshot.config);
                        if desired_hotkey != current_hotkey {
                            if let Err(e) = hotkey.reconfigure(desired_hotkey) {
                                log::warn!("hotkey reconfigure failed: {e:#}");
                            } else {
                                current_hotkey = desired_hotkey;
                            }
                        }
                        match restart_core(&mut client).await {
                            Ok(()) => {
                                overlay.set_state(OverlayState::Idle);
                                log::info!("settings: applied");
                            }
                            Err(e) => {
                                overlay.set_state(OverlayState::Error);
                                show_info(&format!("Settings saved, but core restart failed:\n\n{e}"));
                            }
                        }
                    }
                    Some(SettingsEvent::Cancelled) => {
                        log::info!("settings: cancelled");
                    }
                    Some(SettingsEvent::Failed(message)) => {
                        show_info(&format!("Settings failed:\n\n{message}"));
                    }
                    None => anyhow::bail!("settings channel closed"),
                }
            }
        }
    }
}

async fn shutdown_current_session(
    current: &mut Option<u64>,
    audio: &AudioCapture,
    client: Option<&CoreClient>,
) {
    if let Some(id) = current.take() {
        audio.close_gate();
        if let Some(client) = client {
            let _ = client.cancel(id).await;
        }
    }
}

async fn spawn_core_from_settings() -> Result<CoreClient> {
    let client = CoreClient::spawn(settings::build_spawn_config()?).await?;
    log::info!("handshake complete — core is ready to serve sessions");
    Ok(client)
}

async fn recv_core(client: &Option<CoreClient>) -> Option<Response> {
    match client.as_ref() {
        Some(client) => client.recv().await,
        None => None,
    }
}

async fn restart_core(client: &mut Option<CoreClient>) -> Result<()> {
    let new_client = spawn_core_from_settings().await?;
    if let Some(old_client) = client.replace(new_client) {
        old_client.shutdown().await?;
    }
    Ok(())
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
