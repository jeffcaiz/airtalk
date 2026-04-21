//! Spawn and drive `airtalk-core.exe` over stdio.
//!
//! Responsibilities:
//!
//! - Locate the core binary next to the UI binary.
//! - On Windows, spawn with `CREATE_SUSPENDED`, attach to a Job Object
//!   with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resume. See
//!   DESIGN.md §8 Gotcha #14 — this ordering is load-bearing for
//!   orphan-core prevention.
//! - Wait for `Response::Ready` within 5 s; refuse mismatched
//!   `protocol_version`.
//! - Pump core stderr into the host logger (prefixed `[core]`).
//! - Expose async `begin` / `chunk` / `end` / `cancel` / `recv` / `shutdown`
//!   methods over the NDJSON frame codec.

use airtalk_proto::{read_frame_async, write_frame_async, ProtocolError, Request, Response, PROTOCOL_VERSION};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Parameters for spawning the core child process.
pub struct SpawnConfig {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl SpawnConfig {
    /// Look for `airtalk-core(.exe)` next to the UI executable. Works in
    /// both `cargo run` (target/<profile>/) and packaged installs (both
    /// binaries share one directory).
    pub fn default_sibling() -> Result<Self> {
        let ui = std::env::current_exe().context("std::env::current_exe")?;
        let dir = ui.parent().context("UI exe has no parent directory")?;
        let name = if cfg!(windows) {
            "airtalk-core.exe"
        } else {
            "airtalk-core"
        };
        let exe = dir.join(name);
        if !exe.exists() {
            bail!("core binary not found at {}", exe.display());
        }
        Ok(Self {
            exe,
            args: Vec::new(),
            env: Vec::new(),
        })
    }
}

/// A running core child + the async channels to talk to it.
///
/// All methods take `&self`: internal state is wrapped in `Arc<Mutex<…>>`
/// or `tokio::sync::Mutex<Option<…>>` so a single `CoreClient` can be
/// shared across tasks (send side from audio, recv side from the main
/// event loop, shutdown from a Ctrl-C handler, etc.) without borrow
/// conflicts inside `tokio::select!`.
///
/// `shutdown` is idempotent — calling it twice is a no-op on the second
/// call.
// Most methods land unused until hotkey + audio modules ship. Silence
// the dead-code noise for now — they're the externally-facing API.
#[allow(dead_code)]
pub struct CoreClient {
    next_id: Arc<AtomicU64>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    response_rx: Mutex<Option<mpsc::UnboundedReceiver<Response>>>,
    child: Mutex<Option<Child>>,
    #[cfg(windows)]
    _job: win::JobObject,
}

#[allow(dead_code)]
impl CoreClient {
    pub async fn spawn(cfg: SpawnConfig) -> Result<Self> {
        log::info!("spawning core: {} {:?}", cfg.exe.display(), cfg.args);
        let mut cmd = Command::new(&cfg.exe);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        #[cfg(windows)]
        {
            // CREATE_SUSPENDED = 0x0000_0004, CREATE_NO_WINDOW = 0x0800_0000.
            // tokio::process::Command exposes creation_flags as an inherent
            // method on Windows; no `CommandExt` trait import needed.
            cmd.creation_flags(0x0000_0004 | 0x0800_0000);
        }

        let mut child = cmd.spawn().context("failed to spawn core")?;

        // Windows: attach-to-job MUST happen before resume. DESIGN.md §8 #14.
        #[cfg(windows)]
        let job = {
            let pid = child.id().context("core has no pid")?;
            let raw = child
                .raw_handle()
                .context("tokio Child has no raw handle")?;
            let job = win::create_kill_on_close_job().context("create job")?;
            win::assign_process(&job, windows::Win32::Foundation::HANDLE(raw as _))
                .context("assign process to job")?;
            win::resume_main_thread(pid).context("resume core main thread")?;
            job
        };

        // Forward core stderr lines into our logger. One task, dies on EOF.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump_stderr(stderr));
        }

        let stdin = child.stdin.take().context("stdin piped but missing")?;
        let stdout = child.stdout.take().context("stdout piped but missing")?;
        let mut reader = BufReader::new(stdout);

        // Handshake: Ready must arrive within READY_TIMEOUT.
        let ready: Response = tokio::time::timeout(READY_TIMEOUT, read_frame_async(&mut reader))
            .await
            .context("timed out waiting for core Ready (5 s)")?
            .context("core closed stdout before Ready")?;
        match ready {
            Response::Ready { protocol_version } if protocol_version == PROTOCOL_VERSION => {
                log::info!("core ready (protocol_version={})", protocol_version);
            }
            Response::Ready { protocol_version } => {
                bail!(
                    "protocol version mismatch: core={}, ui={}",
                    protocol_version,
                    PROTOCOL_VERSION
                );
            }
            other => bail!("expected Ready handshake, got {:?}", other),
        }

        // Spin up the response pump. Unbounded because core is our own process —
        // if we're backpressuring stdin below, core won't flood us with responses.
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(pump_responses(reader, tx));

        Ok(Self {
            next_id: Arc::new(AtomicU64::new(1)),
            stdin: Arc::new(Mutex::new(Some(stdin))),
            response_rx: Mutex::new(Some(rx)),
            child: Mutex::new(Some(child)),
            #[cfg(windows)]
            _job: job,
        })
    }

    pub fn next_session_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn send(&self, req: &Request) -> Result<()> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .context("core stdin already closed (shutdown in progress)")?;
        write_frame_async(stdin, req)
            .await
            .context("write frame to core stdin")?;
        Ok(())
    }

    /// Open a session. `vad=true` enables Silero segmentation + concurrent ASR.
    pub async fn begin(&self, vad: bool) -> Result<u64> {
        let id = self.next_session_id();
        self.send(&Request::Begin {
            id,
            vad,
            context: None,
            language: None,
            enable_itn: None,
            enable_llm: None,
        })
        .await?;
        Ok(id)
    }

    pub async fn chunk(&self, id: u64, pcm: Vec<u8>) -> Result<()> {
        self.send(&Request::Chunk { id, pcm }).await
    }

    pub async fn end(&self, id: u64) -> Result<()> {
        self.send(&Request::End { id }).await
    }

    pub async fn cancel(&self, id: u64) -> Result<()> {
        self.send(&Request::Cancel { id }).await
    }

    /// Next response from core. `None` when core stdout closed and the
    /// pump task exited, or after shutdown.
    pub async fn recv(&self) -> Option<Response> {
        let mut guard = self.response_rx.lock().await;
        let rx = guard.as_mut()?;
        rx.recv().await
    }

    /// Graceful shutdown: close stdin (core sees EOF → clean exit), wait
    /// up to [`SHUTDOWN_GRACE`], then force-kill if still alive. Idempotent.
    ///
    /// The Job Object drops last, so even if force-kill somehow misses the
    /// child, OS will take it down when the job handle closes.
    pub async fn shutdown(&self) -> Result<()> {
        // Close stdin: drop the `ChildStdin` by taking it out of the Option.
        // Any other Arc<Mutex<Option<_>>> clones will now see None on send().
        {
            let mut guard = self.stdin.lock().await;
            guard.take();
        }
        // Close the receiver so the response pump task exits.
        {
            let mut guard = self.response_rx.lock().await;
            guard.take();
        }
        // Wait for child. If already taken by a previous shutdown call, nothing to do.
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                Ok(Ok(status)) => log::info!("core exited: {status}"),
                Ok(Err(e)) => log::error!("core wait error: {e}"),
                Err(_) => {
                    log::warn!("core did not exit within {SHUTDOWN_GRACE:?}, killing");
                    let _ = child.kill().await;
                }
            }
        }
        Ok(())
    }
}

async fn pump_stderr<R: AsyncRead + Unpin>(r: R) {
    let reader = BufReader::new(r);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => log::info!("[core] {line}"),
            Ok(None) => break,
            Err(e) => {
                log::warn!("[core] stderr read error: {e}");
                break;
            }
        }
    }
}

async fn pump_responses<R: AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    tx: mpsc::UnboundedSender<Response>,
) where
    BufReader<R>: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        match read_frame_async::<_, Response>(&mut reader).await {
            Ok(resp) => {
                if tx.send(resp).is_err() {
                    // Receiver dropped (CoreClient shutting down).
                    break;
                }
            }
            Err(ProtocolError::Eof) => {
                log::info!("core stdout closed");
                break;
            }
            Err(e) => {
                log::error!("core frame decode error: {e}");
                break;
            }
        }
    }
}

// Compile-time assertion: CoreClient is Send + Sync so it can go in Arc.
#[allow(dead_code)]
fn _assert_send_sync()
where
    CoreClient: Send + Sync,
{
}

#[cfg(windows)]
mod win {
    use anyhow::{bail, Context, Result};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    pub struct JobObject(HANDLE);

    // HANDLE is *mut c_void under the hood. A Job Object handle is safe to
    // move/share across threads — it's just a kernel object ref.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn create_kill_on_close_job() -> Result<JobObject> {
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObjectW")?;
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .context("SetInformationJobObject")?;
            Ok(JobObject(job))
        }
    }

    pub fn assign_process(job: &JobObject, process_handle: HANDLE) -> Result<()> {
        unsafe {
            AssignProcessToJobObject(job.0, process_handle).context("AssignProcessToJobObject")?;
        }
        Ok(())
    }

    /// Find the (single, suspended) main thread of `pid` and resume it.
    ///
    /// A freshly-`CREATE_SUSPENDED`-spawned child has exactly one thread,
    /// so scanning the thread snapshot for the first matching `pid` is safe.
    pub fn resume_main_thread(pid: u32) -> Result<()> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
                .context("CreateToolhelp32Snapshot")?;
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..std::mem::zeroed()
            };
            let first = Thread32First(snap, &mut entry);
            if first.is_err() {
                let _ = CloseHandle(snap);
                bail!("Thread32First returned no threads");
            }
            let mut found = false;
            loop {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                        .context("OpenThread")?;
                    let prev = ResumeThread(thread);
                    let _ = CloseHandle(thread);
                    if prev == u32::MAX {
                        let _ = CloseHandle(snap);
                        bail!("ResumeThread failed");
                    }
                    found = true;
                    break;
                }
                if Thread32Next(snap, &mut entry).is_err() {
                    break;
                }
            }
            let _ = CloseHandle(snap);
            if !found {
                bail!("no thread belongs to pid {pid}");
            }
        }
        Ok(())
    }
}

