//! Black-box integration tests.
//!
//! Spawns the compiled `airtalk-core` binary as a subprocess and
//! drives it through the full NDJSON protocol. DashScope (ASR) and
//! the OpenAI-compatible LLM endpoint are stubbed via `wiremock` so
//! tests need no real API keys or network access.
//!
//! Run with `cargo test -p airtalk-core --test integration`.

use std::process::Stdio;
use std::time::Duration;

use airtalk_proto::{read_frame_async, write_frame_async, Request, Response};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Path to the compiled airtalk-core.exe. Cargo injects this for any
/// `[[bin]]` target when building tests.
const CORE_BIN: &str = env!("CARGO_BIN_EXE_airtalk-core");

/// Wall-clock deadline on any single recv / exit wait. Generous to
/// absorb CI jitter; individual tests assert more specific timings.
const DEADLINE: Duration = Duration::from_secs(30);

const ASR_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";
const LLM_PATH: &str = "/chat/completions";

// ─── Harness ───────────────────────────────────────────────────────────

struct CoreChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CoreChild {
    fn spawn(args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(CORE_BIN);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Let stderr go to the test's own output so panics/warnings
            // are visible when a test fails.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn airtalk-core");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    async fn send(&mut self, req: &Request) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        write_frame_async(stdin, req).await.expect("write frame");
    }

    async fn recv(&mut self) -> Response {
        tokio::time::timeout(DEADLINE, read_frame_async::<_, Response>(&mut self.stdout))
            .await
            .expect("timed out waiting for response")
            .expect("protocol error reading response")
    }

    async fn recv_ready(&mut self) {
        match self.recv().await {
            Response::Ready { protocol_version } => {
                assert_eq!(protocol_version, 3, "unexpected protocol version");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    async fn close_stdin(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.shutdown().await;
            drop(s);
        }
    }

    /// Close stdin and expect the process to exit with code 0 within
    /// [`DEADLINE`]. Consumes `self` so callers can't accidentally
    /// race a final `.recv()`.
    async fn expect_clean_exit(mut self) {
        self.close_stdin().await;
        let status = tokio::time::timeout(DEADLINE, self.child.wait())
            .await
            .expect("timed out waiting for core to exit")
            .expect("wait");
        assert!(status.success(), "core exited non-zero: {status:?}");
    }
}

// ─── Small factories ───────────────────────────────────────────────────

fn begin(id: u64, vad: bool) -> Request {
    Request::Begin {
        id,
        vad,
        context: None,
        language: None,
        enable_itn: None,
        enable_llm: None,
    }
}

fn chunk(id: u64, bytes: usize) -> Request {
    Request::Chunk {
        id,
        pcm: vec![0u8; bytes],
    }
}

fn assert_error(resp: &Response, expected_id: u64, expected_msg: &str) {
    match resp {
        Response::Error { id, message } => {
            assert_eq!(*id, expected_id, "wrong id on Error");
            assert_eq!(message, expected_msg, "wrong error message");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

fn assert_error_prefix(resp: &Response, expected_id: u64, expected_prefix: &str) {
    match resp {
        Response::Error { id, message } => {
            assert_eq!(*id, expected_id, "wrong id on Error");
            assert!(
                message.starts_with(expected_prefix),
                "expected message starting with {expected_prefix:?}, got {message:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ─── Mock servers ──────────────────────────────────────────────────────

async fn mock_asr_ok(text: &str, lang: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ASR_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "output": {
                    "choices": [{
                        "message": {
                            "content": [{"text": text}],
                            "annotations": [{"language": lang}]
                        }
                    }]
                },
                "usage": {
                    "input_tokens_details": {"text_tokens": 0},
                    "output_tokens_details": {"text_tokens": 3},
                    "seconds": 1
                },
                "request_id": "mock"
            })),
        )
        .mount(&server)
        .await;
    server
}

async fn mock_llm_ok(content: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(LLM_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"content": content}
                }],
                "usage": {
                    "prompt_tokens": 30,
                    "completion_tokens": 10,
                    "total_tokens": 40
                }
            })),
        )
        .mount(&server)
        .await;
    server
}

fn asr_endpoint(server: &MockServer) -> String {
    format!("{}{}", server.uri(), ASR_PATH)
}

// ─── Spawn helpers ─────────────────────────────────────────────────────

fn spawn_no_llm() -> CoreChild {
    CoreChild::spawn(
        &["--no-llm", "--log-level", "warn"],
        &[("AIRTALK_ASR_API_KEY", "dummy")],
    )
}

/// Spawn core pointing at mock ASR + mock LLM. `extra_args` appends to
/// the CLI; strings must live until spawn returns (we only borrow them
/// for the duration of the Command build).
fn spawn_with_mocks(asr: &MockServer, llm: &MockServer, extra_args: &[&str]) -> CoreChild {
    let asr_ep = asr_endpoint(asr);
    let llm_ep = llm.uri();
    let mut args: Vec<&str> = vec![
        "--asr-base-url",
        &asr_ep,
        "--llm-base-url",
        &llm_ep,
        "--llm-model",
        "mock-model",
        "--log-level",
        "warn",
    ];
    args.extend_from_slice(extra_args);
    CoreChild::spawn(
        &args,
        &[
            ("AIRTALK_ASR_API_KEY", "mock"),
            ("AIRTALK_LLM_API_KEY", "mock"),
        ],
    )
}

// ─── Tests ─────────────────────────────────────────────────────────────

/// Core emits exactly one Ready at startup with the expected protocol
/// version, then exits cleanly when stdin closes.
#[tokio::test]
async fn ready_handshake_and_clean_exit() {
    let mut core = spawn_no_llm();
    core.recv_ready().await;
    core.expect_clean_exit().await;
}

/// Begin + End with zero chunks surfaces `no_audio` — and the
/// graceful shutdown path still delivers the terminal response.
#[tokio::test]
async fn no_audio_without_chunks() {
    let mut core = spawn_no_llm();
    core.recv_ready().await;
    core.send(&begin(1, false)).await;
    core.send(&Request::End { id: 1 }).await;
    assert_error(&core.recv().await, 1, "no_audio");
    core.expect_clean_exit().await;
}

/// Full happy path: Begin + Chunk + End → ASR mock → LLM mock →
/// Result. Verifies `raw`, `text`, and `language` round-trip.
#[tokio::test]
async fn happy_path_vad_false_with_mocks() {
    let asr = mock_asr_ok("hello world", "en").await;
    let llm = mock_llm_ok("Hello, world.").await;
    let mut core = spawn_with_mocks(&asr, &llm, &[]);
    core.recv_ready().await;

    core.send(&begin(1, false)).await;
    core.send(&chunk(1, 16_000)).await;
    core.send(&Request::End { id: 1 }).await;

    match core.recv().await {
        Response::Result {
            id,
            text,
            raw,
            language,
            stats,
        } => {
            assert_eq!(id, 1);
            assert_eq!(text, "Hello, world.");
            assert_eq!(raw.as_deref(), Some("hello world"));
            assert_eq!(language.as_deref(), Some("en"));
            // 16 KB PCM = 500 ms at 16 kHz/16-bit. vad=false in this
            // test, so vad_segments should be None, asr_calls = 1.
            assert_eq!(stats.pcm_received_ms, 500);
            assert_eq!(stats.pcm_sent_to_asr_ms, 500);
            assert_eq!(stats.vad_segments, None);
            assert_eq!(stats.asr_calls, 1);
            assert!(stats.asr_upload_bytes > 0);
            // ASR + LLM usage should be forwarded from the mocks.
            // DashScope sync-API shape: `seconds` + nested *_details.
            // `total_tokens` is NOT present in sync mode.
            let asr_usage = stats.asr_usage.as_ref().expect("asr_usage");
            assert_eq!(asr_usage.audio_seconds, Some(1.0));
            assert_eq!(asr_usage.output_tokens, Some(3));
            assert_eq!(asr_usage.total_tokens, None);
            let llm_usage = stats.llm_usage.as_ref().expect("llm_usage");
            assert_eq!(llm_usage.prompt_tokens, Some(30));
            assert_eq!(llm_usage.completion_tokens, Some(10));
            assert!(stats.llm_latency_ms.is_some());
        }
        other => panic!("expected Result, got {other:?}"),
    }
    core.expect_clean_exit().await;
}

/// A second Begin while a session is in flight causes the actor to
/// emit `superseded` for the old id and start a fresh pipeline.
#[tokio::test]
async fn supersede_emits_error_then_new_result() {
    let asr = mock_asr_ok("text", "en").await;
    let llm = mock_llm_ok("Text.").await;
    let mut core = spawn_with_mocks(&asr, &llm, &[]);
    core.recv_ready().await;

    core.send(&begin(1, false)).await;
    core.send(&begin(2, false)).await; // preempts id=1

    // First terminal response: the superseded error for id=1.
    assert_error(&core.recv().await, 1, "superseded");

    // id=2 still needs audio + end to finish.
    core.send(&chunk(2, 16_000)).await;
    core.send(&Request::End { id: 2 }).await;

    match core.recv().await {
        Response::Result { id, stats, .. } => {
            assert_eq!(id, 2);
            // Sanity: the fresh pipeline still populated stats.
            assert_eq!(stats.asr_calls, 1);
        }
        other => panic!("expected Result for id=2, got {other:?}"),
    }
    core.expect_clean_exit().await;
}

/// Explicit Cancel surfaces the `cancelled` error code.
#[tokio::test]
async fn cancel_returns_cancelled() {
    let mut core = spawn_no_llm();
    core.recv_ready().await;
    core.send(&begin(1, false)).await;
    core.send(&Request::Cancel { id: 1 }).await;
    assert_error(&core.recv().await, 1, "cancelled");
    core.expect_clean_exit().await;
}

/// Chunks whose id doesn't match the current session are silently
/// dropped on the core side — the current session ends up with no
/// audio and surfaces `no_audio`.
#[tokio::test]
async fn stale_chunk_for_wrong_id_is_dropped() {
    let mut core = spawn_no_llm();
    core.recv_ready().await;
    core.send(&begin(1, false)).await;
    core.send(&chunk(99, 16_000)).await; // stale
    core.send(&Request::End { id: 1 }).await;
    assert_error(&core.recv().await, 1, "no_audio");
    core.expect_clean_exit().await;
}

/// If stdin EOFs while a pipeline is still waiting on ASR/LLM, the
/// actor must drain the in-flight session, emit its Result, then
/// exit cleanly. Without graceful shutdown the process hangs.
#[tokio::test]
async fn graceful_shutdown_delivers_inflight_result() {
    // ASR responds after 1s; stdin will close before then.
    let asr_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ASR_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(json!({
                    "output": {
                        "choices": [{
                            "message": {
                                "content": [{"text": "delayed"}],
                                "annotations": [{"language": "en"}]
                            }
                        }]
                    }
                })),
        )
        .mount(&asr_server)
        .await;
    let llm = mock_llm_ok("Delayed.").await;
    let mut core = spawn_with_mocks(&asr_server, &llm, &[]);
    core.recv_ready().await;

    core.send(&begin(1, false)).await;
    core.send(&chunk(1, 16_000)).await;
    core.send(&Request::End { id: 1 }).await;
    core.close_stdin().await; // <-- EOF while ASR is still running

    match core.recv().await {
        Response::Result { id, .. } => assert_eq!(id, 1),
        // (graceful shutdown path: we're more interested in *whether*
        // we get a Result than in its stats, so no stat assertions here.)
        other => panic!("expected Result after graceful shutdown, got {other:?}"),
    }

    // Process should exit on its own after the pipeline finishes.
    let status = tokio::time::timeout(DEADLINE, core.child.wait())
        .await
        .expect("timed out waiting for exit")
        .expect("wait");
    assert!(status.success(), "core exited non-zero: {status:?}");
}

/// ASR HTTP 500 surfaces as `asr_failed:` with the upstream message
/// appended (prefix match — the detail is free-form).
#[tokio::test]
async fn asr_500_surfaces_asr_failed_prefix() {
    let asr_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ASR_PATH))
        .respond_with(ResponseTemplate::new(500).set_body_string("service boom"))
        .mount(&asr_server)
        .await;
    let llm = mock_llm_ok("x").await;
    let mut core = spawn_with_mocks(&asr_server, &llm, &[]);
    core.recv_ready().await;

    core.send(&begin(1, false)).await;
    core.send(&chunk(1, 16_000)).await;
    core.send(&Request::End { id: 1 }).await;

    assert_error_prefix(&core.recv().await, 1, "asr_failed:");
    core.expect_clean_exit().await;
}

/// A reqwest client-side timeout (delay longer than `--asr-timeout-ms`)
/// surfaces as the dedicated `timeout` error code, not the generic
/// `asr_failed:` prefix.
#[tokio::test]
async fn asr_timeout_surfaces_timeout_code() {
    let asr_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ASR_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(json!({"output": {"choices": []}})),
        )
        .mount(&asr_server)
        .await;
    let llm = mock_llm_ok("x").await;
    let mut core = spawn_with_mocks(&asr_server, &llm, &["--asr-timeout-ms", "300"]);
    core.recv_ready().await;

    core.send(&begin(1, false)).await;
    core.send(&chunk(1, 16_000)).await;
    core.send(&Request::End { id: 1 }).await;

    assert_error(&core.recv().await, 1, "timeout");
    core.expect_clean_exit().await;
}
