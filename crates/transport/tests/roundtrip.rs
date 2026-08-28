//! The wire, end to end: framed JSON-RPC driving `serve` against a real
//! session registry whose sessions host a real child process.
//!
//! `serve` is generic over its streams, so this suite runs it over an
//! in-process duplex with the framed [`Client`] on the other side — the JSON
//! peer is in-process, but the part that most needs to be real, the hosted
//! CLI child, is a genuine subprocess: this same test binary re-invoked in a
//! fixture role, the pattern the terminal and session suites already use. The
//! binary runs its own `main` (`harness = false`) so it can become that
//! fixture when handed the role marker, and otherwise run the checks below.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use agent_bridge_core::{
    AdapterSeam, BusConfig, CreateOptions, EventBus, InputStep, LaunchSpec, RegistryConfig,
    SessionConfig, SessionRegistry, ShutdownHint,
};
use agent_bridge_transport::{
    Client, Message, RuntimeContext, RuntimeInfoRef, ServeControl, ServeOutcome, defaults, encode,
    serve,
};
use serde_json::{Value, json};

/// The environment marker that turns a re-invocation of this binary into the
/// fixture child rather than the test runner.
const FIXTURE_ENV: &str = "AGENT_BRIDGE_TRANSPORT_FIXTURE_ROLE";

/// How long a check waits for a message it expects before calling the run
/// stalled. Generous — a loaded machine is not a failing transport.
const PATIENCE: Duration = Duration::from_secs(15);

/// The outer bound on a whole check. `Client::call` awaits its matching
/// response with no deadline of its own, so a dispatcher regression that
/// dropped a response would hang this binary — and with it `cargo test
/// --workspace` — rather than reporting a failed step. Bounding each step
/// turns that hang into a counted failure. Set clear of `PATIENCE` so a step's
/// own internal deadlines fire first when the fault is a slow message rather
/// than a missing one.
const STEP_BUDGET: Duration = Duration::from_secs(45);

/// One check's boxed future — a named alias so the check table is not a wall
/// of nested generics.
type CheckFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>>;

/// One entry in the check table: a name and the thunk that runs it.
type Check = (&'static str, fn() -> CheckFuture);

fn main() {
    if std::env::var_os(FIXTURE_ENV).is_some() {
        fixture_child();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a runtime for the checks");

    let checks: &[Check] = &[
        ("mvp_roundtrip", || Box::pin(mvp_roundtrip())),
        ("wrong_approval_id_returns_32007", || {
            Box::pin(wrong_approval_id_returns_32007())
        }),
        ("a_reason_without_a_deny_is_refused", || {
            Box::pin(a_reason_without_a_deny_is_refused())
        }),
        ("unknown_adapter_and_session_codes", || {
            Box::pin(unknown_adapter_and_session_codes())
        }),
        ("method_name_cap_rejects_before_dispatch", || {
            Box::pin(method_name_cap_rejects_before_dispatch())
        }),
        ("oversized_frame_closes_the_transport", || {
            Box::pin(oversized_frame_closes_the_transport())
        }),
        (
            "a_protocol_close_emits_no_session_frames_after_the_terminal_error",
            || Box::pin(a_protocol_close_emits_no_session_frames_after_the_terminal_error()),
        ),
        ("runtime_shutdown_drains_and_ends", || {
            Box::pin(runtime_shutdown_drains_and_ends())
        }),
        ("attached_subscriber_sees_final_events_on_shutdown", || {
            Box::pin(attached_subscriber_sees_final_events_on_shutdown())
        }),
        ("attach_schema_version_gate", || {
            Box::pin(attach_schema_version_gate())
        }),
        ("a_repeated_attach_is_idempotent", || {
            Box::pin(a_repeated_attach_is_idempotent())
        }),
        (
            "notification_draws_no_response_and_params_are_strict",
            || Box::pin(notification_draws_no_response_and_params_are_strict()),
        ),
        ("a_shutdown_notification_takes_effect", || {
            Box::pin(a_shutdown_notification_takes_effect())
        }),
    ];

    let mut failed = 0;
    for (name, check) in checks {
        let outcome = runtime.block_on(async {
            match tokio::time::timeout(STEP_BUDGET, check()).await {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "made no progress within {STEP_BUDGET:?} — the runtime never delivered a response"
                )),
            }
        });
        match outcome {
            Ok(()) => eprintln!("roundtrip step={name} status=pass"),
            Err(detail) => {
                failed += 1;
                eprintln!(
                    "roundtrip step={name} status=fail detail=\"{}\"",
                    detail.replace('"', "'")
                );
            }
        }
    }
    eprintln!("roundtrip checks={} failed={failed}", checks.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

/// The fixture CLI: wait for a nudge on stdin, announce readiness (which the
/// runtime observes as first output and so `Running`), then echo nothing and
/// exit on `exit` or on end of input. Deliberately quiet until nudged so a
/// test can attach *before* the `Running` transition it wants to observe.
fn fixture_child() -> ! {
    let stdin = std::io::stdin();
    let mut line = String::new();
    // Block until the first input arrives, so readiness is emitted after the
    // test has had its chance to subscribe.
    let _ = stdin.lock().read_line(&mut line);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"ready\n");
    let _ = stdout.flush();
    // A close before the normal exchange delivers `exit` as this very first
    // line; having announced readiness, exit now rather than looping for a
    // second line a closed session never sends — which would otherwise stall
    // the check through the full drain grace before termination.
    if line.trim() == "exit" {
        std::process::exit(0);
    }
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => std::process::exit(0),
            Ok(_) => {
                if line.trim() == "exit" {
                    std::process::exit(0);
                }
            }
        }
    }
}

/// The adapter every check launches: this binary, re-invoked in its fixture
/// role. A real child in a real terminal, deterministic across platforms.
struct FixtureAdapter;

impl AdapterSeam for FixtureAdapter {
    fn launch_spec(&self, _options: &CreateOptions) -> LaunchSpec {
        let mut launch = LaunchSpec::new(std::env::current_exe().expect("the test binary's path"));
        launch.env = vec![(FIXTURE_ENV.to_string(), "1".to_string())];
        launch
    }

    fn shutdown_hint(&self) -> ShutdownHint {
        ShutdownHint::Input(vec![InputStep::Write("exit\n".into())])
    }
}

/// A per-check temporary directory for session logs.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-bridge-transport-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch dir");
    dir
}

/// A serving runtime over an in-process duplex, with the framed client on the
/// other end and a handle to end it.
struct Harness {
    client: Client<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    shutdown: tokio::sync::watch::Sender<bool>,
    serve: tokio::task::JoinHandle<ServeOutcome>,
    intent: Arc<std::sync::atomic::AtomicBool>,
}

impl Harness {
    fn start(tag: &str) -> Self {
        let bus = EventBus::new(BusConfig::default());
        let registry = SessionRegistry::new(
            bus.clone(),
            RegistryConfig::new(SessionConfig::new(scratch_dir(tag))),
        );
        registry.register_adapter("fixture", Arc::new(FixtureAdapter));
        let ctx = RuntimeContext {
            registry,
            bus,
            info: RuntimeInfoRef {
                version: "0.0.1-test".into(),
                adapters: vec!["fixture".into()],
                capabilities: vec!["session.attach".into()],
                schema_version: agent_bridge_events::SCHEMA_VERSION,
            },
        };
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = tokio::io::split(server_end);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (shutdown, _initial_rx) = tokio::sync::watch::channel(false);
        let intent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let intent_hook = Arc::clone(&intent);
        let control = ServeControl {
            shutdown: shutdown.clone(),
            drain_grace: Duration::from_secs(10),
            stdout_deadline: Duration::from_secs(5),
            max_frame_bytes: defaults::MAX_FRAME_BYTES,
        };
        let serve = tokio::spawn(async move {
            serve(ctx, server_read, server_write, control, move || {
                intent_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
        });
        let client = Client::new(client_read, client_write, defaults::MAX_FRAME_BYTES);
        Self {
            client,
            shutdown,
            serve,
            intent,
        }
    }

    /// Stop the runtime and await its outcome.
    async fn stop(self) -> ServeOutcome {
        let _ = self.shutdown.send(true);
        tokio::time::timeout(PATIENCE, self.serve)
            .await
            .expect("serve must end within patience")
            .expect("the serve task must not panic")
    }

    /// Read notifications until one is `session.event` of `event_type`, or
    /// time out. Buffered notifications from a prior `call` are consulted
    /// first.
    async fn wait_for_event(&mut self, event_type: &str) -> Result<Value, String> {
        // `next` pops one buffered message at a time, so a later event still
        // in the buffer (a `session.eof` sitting behind the `closed` this call
        // is looking for) survives for the next reader rather than being
        // drained away with the rest.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let next = tokio::time::timeout(remaining, self.client.next())
                .await
                .map_err(|_| format!("timed out waiting for {event_type}"))?
                .map_err(|error| format!("framing error waiting for {event_type}: {error}"))?
                .ok_or_else(|| format!("stream ended before {event_type}"))?;
            if let Some(params) = event_of_type(&next, event_type) {
                return Ok(params);
            }
        }
    }
}

/// The event params if `message` is a `session.event` of `event_type`.
fn event_of_type(message: &Message, event_type: &str) -> Option<Value> {
    match message {
        Message::Notification { method, params } if method == "session.event" => {
            (params.get("type").and_then(Value::as_str) == Some(event_type)).then(|| params.clone())
        }
        _ => None,
    }
}

/// The MVP ladder over a real subprocess: info, create, attach, a nudge that
/// drives the child to `Running`, then a graceful close whose lifecycle
/// events arrive as notifications and whose subscription ends in
/// `session.eof`.
async fn mvp_roundtrip() -> Result<(), String> {
    let mut h = Harness::start("mvp");

    let info = h
        .client
        .call(json!(1), "runtime.info", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("runtime.info errored: {e}"))?;
    if info["schema_version"] != json!(agent_bridge_events::SCHEMA_VERSION) {
        return Err(format!("runtime.info schema_version wrong: {info}"));
    }
    if info["adapters"] != json!(["fixture"]) {
        return Err(format!("runtime.info adapters wrong: {info}"));
    }

    let created = h
        .client
        .call(json!(2), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"]
        .as_str()
        .ok_or("create returned no session_id")?
        .to_string();

    h.client
        .call(
            json!(3),
            "session.attach",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("attach errored: {e}"))?;

    // Nudge the child so it announces readiness — observed as `Running` — on a
    // subscription that is now live, so the transition cannot be missed.
    h.client
        .call(
            json!(4),
            "session.send",
            json!({ "session_id": session_id, "input": "go\n" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("send errored: {e}"))?;
    h.wait_for_event("lifecycle.session.running").await?;

    h.client
        .call(
            json!(5),
            "session.close",
            json!({ "session_id": session_id, "force": true }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("close errored: {e}"))?;

    let closed = h.wait_for_event("lifecycle.session.closed").await?;
    if closed["session_id"].as_str() != Some(session_id.as_str()) {
        return Err(format!("closed event on the wrong session: {closed}"));
    }

    // The subscription ends with a session.eof naming the closed session.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(remaining, h.client.next())
            .await
            .map_err(|_| "timed out waiting for session.eof".to_string())?
            .map_err(|e| format!("framing error: {e}"))?
            .ok_or("stream ended before session.eof")?;
        if let Message::Notification { method, params } = &next
            && method == "session.eof"
        {
            if params["reason"] != json!("session_closed") {
                return Err(format!("session.eof wrong reason: {params}"));
            }
            break;
        }
    }

    if h.stop().await != ServeOutcome::Drained {
        return Err("runtime did not drain cleanly".into());
    }
    Ok(())
}

/// A `resolve_approval` with an id that matches nothing, on a `Running`
/// session, returns `-32007` and does not disturb the session.
async fn wrong_approval_id_returns_32007() -> Result<(), String> {
    let mut h = Harness::start("approval");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();
    h.client
        .call(
            json!(2),
            "session.attach",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("attach errored: {e}"))?;
    h.client
        .call(
            json!(3),
            "session.send",
            json!({ "session_id": session_id, "input": "go\n" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("send errored: {e}"))?;
    h.wait_for_event("lifecycle.session.running").await?;

    let error = h
        .client
        .call(
            json!(4),
            "session.resolve_approval",
            json!({ "session_id": session_id, "approval_id": "a-nonesuch", "decision": "allow" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("a stale approval id must error");
    if error["code"] != json!(-32007) {
        return Err(format!("expected -32007, got {error}"));
    }
    let _ = h.stop().await;
    Ok(())
}

/// A `resolve_approval` carrying a `reason` with a non-deny decision is
/// refused with `-32602`: a reason explains a denial, and the strict-parameter
/// contract will not silently drop it for `allow` or `ask`.
async fn a_reason_without_a_deny_is_refused() -> Result<(), String> {
    let mut h = Harness::start("reason");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    let error = h
        .client
        .call(
            json!(2),
            "session.resolve_approval",
            json!({
                "session_id": session_id,
                "approval_id": "a-1",
                "decision": "allow",
                "reason": "not applicable to an allow",
            }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("a reason with a non-deny decision must be refused");
    if error["code"] != json!(-32602) {
        return Err(format!("expected -32602, got {error}"));
    }
    let _ = h.stop().await;
    Ok(())
}

/// The three code paths for a session a caller names wrongly: an unknown
/// adapter, a well-formed but unknown id, and an id that is not an id.
async fn unknown_adapter_and_session_codes() -> Result<(), String> {
    let mut h = Harness::start("codes");

    let unknown_adapter = h
        .client
        .call(
            json!(1),
            "session.create",
            json!({ "adapter": "no-such-adapter" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("an unknown adapter must error");
    if unknown_adapter["code"] != json!(-32001) {
        return Err(format!(
            "unknown adapter: expected -32001, got {unknown_adapter}"
        ));
    }

    let unknown_session = h
        .client
        .call(
            json!(2),
            "session.interrupt",
            json!({ "session_id": "00000000-0000-4000-8000-000000000000" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("an unknown session must error");
    if unknown_session["code"] != json!(-32002) {
        return Err(format!(
            "unknown session: expected -32002, got {unknown_session}"
        ));
    }

    let malformed = h
        .client
        .call(
            json!(3),
            "session.interrupt",
            json!({ "session_id": "not-a-uuid" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("a malformed id must error");
    if malformed["code"] != json!(-32602) {
        return Err(format!("malformed id: expected -32602, got {malformed}"));
    }

    let _ = h.stop().await;
    Ok(())
}

/// A method name past the cap is rejected as method-not-found before any
/// dispatch, not parsed as a giant unknown name.
async fn method_name_cap_rejects_before_dispatch() -> Result<(), String> {
    let mut h = Harness::start("methodcap");
    let overlong = "x".repeat(129);
    let error = h
        .client
        .call(json!(1), &overlong, json!({}))
        .await
        .map_err(|e| e.to_string())?
        .expect_err("an overlong method must error");
    if error["code"] != json!(-32601) {
        return Err(format!("expected -32601, got {error}"));
    }
    let _ = h.stop().await;
    Ok(())
}

/// A declared `Content-Length` past the frame cap draws a `transport.error`
/// of code `frame_too_large` and closes the transport.
async fn oversized_frame_closes_the_transport() -> Result<(), String> {
    let mut h = Harness::start("oversize");
    // Write a raw header declaring a body far past the cap. The reader refuses
    // it on the header alone, before any body.
    let header = format!("Content-Length: {}\r\n\r\n", defaults::MAX_FRAME_BYTES + 1);
    h.client
        .send_raw(header.as_bytes())
        .await
        .map_err(|e| format!("raw write failed: {e}"))?;

    let deadline = tokio::time::Instant::now() + PATIENCE;
    let mut saw_error = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, h.client.next()).await {
            Err(_) => return Err("timed out waiting for frame_too_large".into()),
            Ok(Ok(Some(message))) => {
                if let Some(params) = event_of_type(&message, "transport.error")
                    && params["payload"]["code"] == json!("frame_too_large")
                {
                    saw_error = true;
                }
            }
            // The transport closed after emitting the error — the expected end.
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(format!("framing error: {error}")),
        }
    }
    if !saw_error {
        return Err("no frame_too_large transport.error before close".into());
    }
    let outcome = tokio::time::timeout(PATIENCE, h.serve)
        .await
        .map_err(|_| "serve did not end".to_string())?
        .map_err(|_| "serve panicked".to_string())?;
    if outcome != ServeOutcome::ProtocolClosed {
        return Err(format!("expected ProtocolClosed, got {outcome:?}"));
    }
    Ok(())
}

/// A notification (a call with no id) is not answered, and a parameterless
/// method refuses unexpected params — the strict stance the typed methods take.
async fn notification_draws_no_response_and_params_are_strict() -> Result<(), String> {
    let mut h = Harness::start("notif");

    // A well-formed notification: no id. It must draw no response.
    let notification = json!({ "jsonrpc": "2.0", "method": "runtime.info" });
    h.client
        .send_raw(&encode(&serde_json::to_vec(&notification).unwrap()))
        .await
        .map_err(|e| format!("raw write failed: {e}"))?;

    // A real request follows; if the notification had drawn a response, it would
    // have been buffered here while we waited for this id.
    let info = h
        .client
        .call(json!(1), "runtime.info", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("runtime.info errored: {e}"))?;
    if info.get("version").is_none() {
        return Err(format!("runtime.info result malformed: {info}"));
    }
    if !h.client.take_buffered().is_empty() {
        return Err("the notification drew a response it should not have".into());
    }

    // Unexpected params on a parameterless method are refused, not ignored.
    let error = h
        .client
        .call(json!(2), "runtime.info", json!({ "unexpected": true }))
        .await
        .map_err(|e| e.to_string())?
        .expect_err("unexpected params must be refused");
    if error["code"] != json!(-32602) {
        return Err(format!(
            "expected -32602 for unexpected params, got {error}"
        ));
    }

    let _ = h.stop().await;
    Ok(())
}

/// On a protocol close, the terminal `transport.error` is the last session
/// frame the peer sees: an attached forwarder's `session.event` / `session.eof`
/// must not trail it. The non-operator path aborts the forwarders before
/// draining, so draining a session cannot enqueue frames after the terminal.
async fn a_protocol_close_emits_no_session_frames_after_the_terminal_error() -> Result<(), String> {
    let mut h = Harness::start("terminal-order");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();
    h.client
        .call(
            json!(2),
            "session.attach",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("attach errored: {e}"))?;

    // Trigger a protocol close with an oversized frame header.
    let header = format!("Content-Length: {}\r\n\r\n", defaults::MAX_FRAME_BYTES + 1);
    h.client
        .send_raw(header.as_bytes())
        .await
        .map_err(|e| format!("raw write failed: {e}"))?;

    let mut saw_error = false;
    let mut session_frame_after_error = false;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, h.client.next()).await {
            Err(_) => return Err("timed out draining after the terminal error".into()),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(format!("framing error: {error}")),
            Ok(Ok(Some(message))) => {
                let is_terminal = event_of_type(&message, "transport.error")
                    .is_some_and(|params| params["payload"]["code"] == json!("frame_too_large"));
                if is_terminal {
                    saw_error = true;
                } else if saw_error
                    && let Message::Notification { method, .. } = &message
                    && (method == "session.event" || method == "session.eof")
                {
                    session_frame_after_error = true;
                }
            }
        }
    }
    if !saw_error {
        return Err("no frame_too_large transport.error before close".into());
    }
    if session_frame_after_error {
        return Err("a session frame trailed the terminal transport.error".into());
    }
    Ok(())
}

/// A second `session.attach` for a session this connection already holds is
/// idempotent: it acknowledges the same session and creates no second
/// subscription, so a repeated attach cannot fan out duplicate forwarders. A
/// duplicated subscription would end twice, so counting one `session.eof` at
/// shutdown is the proof.
async fn a_repeated_attach_is_idempotent() -> Result<(), String> {
    let mut h = Harness::start("reattach");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    for id in [2, 3] {
        let ack = h
            .client
            .call(
                json!(id),
                "session.attach",
                json!({ "session_id": session_id }),
            )
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("attach {id} errored: {e}"))?;
        if ack["session_id"].as_str() != Some(session_id.as_str()) {
            return Err(format!("attach {id} acknowledged the wrong session: {ack}"));
        }
    }

    h.client
        .call(json!(4), "runtime.shutdown", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("shutdown errored: {e}"))?;

    let mut eofs = 0;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, h.client.next()).await {
            Err(_) => return Err("timed out counting session.eof frames".into()),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(format!("framing error: {error}")),
            Ok(Ok(Some(Message::Notification { method, .. }))) if method == "session.eof" => {
                eofs += 1;
            }
            Ok(Ok(Some(_))) => {}
        }
    }
    if eofs != 1 {
        return Err(format!("expected exactly one session.eof, got {eofs}"));
    }
    Ok(())
}

/// A notification (a call with no id) runs its method: a `runtime.shutdown`
/// notification drains the runtime and ends the serve loop, though it draws no
/// response — where ignoring notifications outright would have left it running.
async fn a_shutdown_notification_takes_effect() -> Result<(), String> {
    let mut h = Harness::start("notif-shutdown");
    let shutdown = json!({ "jsonrpc": "2.0", "method": "runtime.shutdown" });
    h.client
        .send_raw(&encode(&serde_json::to_vec(&shutdown).unwrap()))
        .await
        .map_err(|e| format!("raw write failed: {e}"))?;
    let outcome = tokio::time::timeout(PATIENCE, h.serve)
        .await
        .map_err(|_| "the shutdown notification did not end serve".to_string())?
        .map_err(|_| "serve panicked".to_string())?;
    if outcome != ServeOutcome::Drained {
        return Err(format!(
            "expected Drained after a shutdown notification, got {outcome:?}"
        ));
    }
    Ok(())
}

/// `session.attach` honors a pinned `expected_schema_version`: the runtime's
/// own version attaches, a different one is refused `-32008` before any events
/// flow.
async fn attach_schema_version_gate() -> Result<(), String> {
    let mut h = Harness::start("schema-gate");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    let current = agent_bridge_events::SCHEMA_VERSION;
    h.client
        .call(
            json!(2),
            "session.attach",
            json!({ "session_id": session_id, "expected_schema_version": current }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("matching schema_version must attach, got {e}"))?;

    let error = h
        .client
        .call(
            json!(3),
            "session.attach",
            json!({ "session_id": session_id, "expected_schema_version": current + 1 }),
        )
        .await
        .map_err(|e| e.to_string())?
        .expect_err("a mismatched schema_version must be refused");
    if error["code"] != json!(-32008) {
        return Err(format!("expected -32008, got {error}"));
    }
    let _ = h.stop().await;
    Ok(())
}

/// The shutdown-flush contract: a subscriber attached to a live session
/// receives that session's final events and the closing `session.eof` when the
/// runtime is shut down. The drain joins the forwarders and lets them flush
/// rather than aborting them, so this holds deterministically — this test
/// guards that path (a reordering that reclaimed the writer before the
/// forwarders finished, or dropped the eof, would break it).
async fn attached_subscriber_sees_final_events_on_shutdown() -> Result<(), String> {
    let mut h = Harness::start("shutdown-attached");
    let created = h
        .client
        .call(json!(1), "session.create", json!({ "adapter": "fixture" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("create errored: {e}"))?;
    let session_id = created["session_id"].as_str().unwrap().to_string();
    h.client
        .call(
            json!(2),
            "session.attach",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("attach errored: {e}"))?;
    h.client
        .call(
            json!(3),
            "session.send",
            json!({ "session_id": session_id, "input": "go\n" }),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("send errored: {e}"))?;
    h.wait_for_event("lifecycle.session.running").await?;

    // Shut the runtime down WITHOUT closing the session first: the drain must
    // close it and the forwarder must deliver its closed event and eof before
    // the wire goes away.
    h.client
        .call(json!(4), "runtime.shutdown", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("shutdown errored: {e}"))?;

    let (mut closed_exit_code, mut eof) = (None, None);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, h.client.next()).await {
            Err(_) => return Err("timed out waiting for the session's final frames".into()),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(format!("framing error: {error}")),
            Ok(Ok(Some(message))) => {
                if let Some(params) = event_of_type(&message, "lifecycle.session.closed") {
                    closed_exit_code = Some(params["payload"].get("exit_code").cloned());
                }
                if let Message::Notification { method, params } = &message
                    && method == "session.eof"
                {
                    eof = Some(params.clone());
                }
            }
        }
    }
    let Some(closed_exit_code) = closed_exit_code else {
        return Err("attached subscriber missed lifecycle.session.closed on shutdown".into());
    };
    let Some(eof) = eof else {
        return Err("attached subscriber missed session.eof on shutdown".into());
    };
    // The eof must echo the closed event's exit code — proving the forwarder
    // carried it end to end. The value itself is platform-dependent: a graceful
    // `exit` hint exits 0, while a close that escalates to termination (as when
    // the hint does not complete within the drain window on a slow runner)
    // reports the terminated code, so the test asserts the two agree rather than
    // a fixed value.
    let eof_exit_code = eof.get("exit_code").cloned();
    if eof["reason"] != json!("session_closed") || eof_exit_code != closed_exit_code {
        return Err(format!(
            "session.eof exit_code {eof_exit_code:?} does not match the closed event {closed_exit_code:?}: {eof}"
        ));
    }
    let outcome = tokio::time::timeout(PATIENCE, h.serve)
        .await
        .map_err(|_| "serve did not end".to_string())?
        .map_err(|_| "serve panicked".to_string())?;
    if outcome != ServeOutcome::Drained {
        return Err(format!("expected Drained, got {outcome:?}"));
    }
    Ok(())
}

/// `runtime.shutdown` acknowledges, then drains and ends — with the operator
/// intent recorded before the drain.
async fn runtime_shutdown_drains_and_ends() -> Result<(), String> {
    let mut h = Harness::start("shutdown");
    let ack = h
        .client
        .call(json!(1), "runtime.shutdown", json!({}))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("shutdown errored: {e}"))?;
    if ack["ok"] != json!(true) {
        return Err(format!("shutdown ack wrong: {ack}"));
    }
    let outcome = tokio::time::timeout(PATIENCE, h.serve)
        .await
        .map_err(|_| "serve did not end after shutdown".to_string())?
        .map_err(|_| "serve panicked".to_string())?;
    if outcome != ServeOutcome::Drained {
        return Err(format!("expected Drained, got {outcome:?}"));
    }
    if !h.intent.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("operator intent was not recorded before drain".into());
    }
    Ok(())
}
