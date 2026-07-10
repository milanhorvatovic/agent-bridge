//! Hook-side plumbing for the live lane: the generated `--settings` JSON
//! that registers this probe as Claude Code's hook command, the throwaway
//! IPC listener the probe runs to receive those hook payloads, and the
//! `hook-forward` client mode the CLI actually executes per hook event.
//!
//! Wire shape (all verified against Claude Code 2.1.x): each registered
//! hook command receives its event payload as one JSON object on stdin
//! (`hook_event_name` discriminates), and a `PreToolUse` hook can decide
//! the pending tool call by printing
//! `{"hookSpecificOutput": {"hookEventName": "PreToolUse",
//! "permissionDecision": "allow"|"deny"|"ask", ...}}` to stdout. The hook
//! process always exits 0 — the decision travels in the JSON body, and a
//! forwarding failure must degrade to "no opinion", never wedge the
//! session.
//!
//! The listener protocol between `hook-forward` and the probe is one JSON
//! line each way: payload in, `{"decision": "...", "reason": "..."}` back.
//! POSIX uses a blocking Unix domain socket; Windows uses a tokio named
//! pipe (`\\.\pipe\...`), looping reads against the runtime's per-read cap
//! and pre-creating the next server instance before serving a connection so
//! a concurrent hook never finds no listener. This listener is deliberately
//! throwaway probe harness, not the runtime's durable approver.

use std::io::{Read, Write};
// Line-oriented reads are the POSIX transport's business; the named-pipe
// path loops raw reads against the per-read cap instead.
#[cfg(unix)]
use std::io::BufRead;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The hook events this probe registers. `SessionStart` advertises the
/// transcript path, `PreToolUse` carries the approval round-trip,
/// `SessionEnd` proves `/exit` landed, `Notification` surfaces the
/// permission dialog the `ask` decision degrades to, and `Stop` marks each
/// turn's end. `PreCompact` costs nothing to observe and completes the
/// lifecycle picture.
///
/// `PostToolUse` earns its place by making one assertion possible that is
/// otherwise only inferable: a **denied** tool call fires `PreToolUse` and
/// then *no* `PostToolUse`, where an allowed one fires both. That is how the
/// approval round-trip is verified by event shape rather than by reading the
/// model's prose about being blocked.
pub const HOOK_EVENTS: [&str; 7] = [
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "SessionEnd",
    "Notification",
    "Stop",
    "PreCompact",
];

/// The tool-lifecycle hooks need a matcher (empty = match every tool) and an
/// explicit timeout, because the CLI blocks on their reply.
const TOOL_HOOKS: [&str; 2] = ["PreToolUse", "PostToolUse"];

/// How long the CLI waits for a tool hook's reply before proceeding without
/// it, in seconds.
const TOOL_HOOK_TIMEOUT_SECS: u32 = 30;

/// The settings JSON injected via `--settings`: every event routed to the
/// same command.
pub fn settings_json(hook_command: &str) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        let mut entry = serde_json::json!({
            "hooks": [{"type": "command", "command": hook_command}],
        });
        if TOOL_HOOKS.contains(&event) {
            entry["matcher"] = serde_json::json!("");
            entry["hooks"][0]["timeout"] = serde_json::json!(TOOL_HOOK_TIMEOUT_SECS);
        }
        hooks.insert(event.to_string(), serde_json::json!([entry]));
    }
    serde_json::json!({"hooks": hooks})
}

/// The command line registered for every hook: this same probe binary in
/// `hook-forward` mode, pointed at the session's listener endpoint. Paths
/// are double-quoted for the shell (POSIX) / cmd (Windows) that runs hook
/// commands.
pub fn hook_command(probe_exe: &Path, endpoint: &str) -> String {
    format!(
        "\"{}\" hook-forward --endpoint \"{}\"",
        probe_exe.display(),
        endpoint
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// No opinion: the CLI's own permission flow applies.
    NoOpinion,
    Allow,
    Deny,
    Ask,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::NoOpinion => "none",
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        }
    }
}

/// A hook payload as received by the listener, timestamped on arrival.
pub struct HookEvent {
    pub name: String,
    pub payload: serde_json::Value,
    pub at: Instant,
}

/// The probe-side listener: hook payloads arrive on `events`; the reply to
/// the *next* `PreToolUse` is whatever `set_decision` last chose. The
/// accept loop runs on detached threads for the probe's whole life — the
/// process exit reaps them, the same deliberate leak policy as the PTY
/// helper threads.
pub struct HookListener {
    pub events: mpsc::Receiver<HookEvent>,
    endpoint: String,
    decision: Arc<Mutex<Decision>>,
}

impl HookListener {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Choose what the listener answers to PreToolUse payloads from now on.
    pub fn set_decision(&self, decision: Decision) {
        *self.decision.lock().unwrap() = decision;
    }
}

/// The one-line reply the listener sends for any payload: the currently
/// configured decision. Only PreToolUse consumers act on it; other hooks
/// read it as an acknowledgement.
fn reply_for(decision: Decision) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "decision": decision.as_str(),
            "reason": format!("interactive-probe external approver: {}", decision.as_str()),
        })
    )
}

fn event_from(payload_line: &str, at: Instant) -> HookEvent {
    let payload: serde_json::Value = serde_json::from_str(payload_line).unwrap_or_else(|_| {
        // A malformed payload is still an observation worth surfacing to
        // the probe rather than dropping on the floor.
        serde_json::json!({"hook_event_name": "unparseable", "raw": payload_line})
    });
    let name = payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    HookEvent { name, payload, at }
}

#[cfg(unix)]
pub fn start_listener(workdir: &Path, _session_tag: &str) -> std::io::Result<HookListener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let socket_path = workdir.join("hook.sock");
    let listener = UnixListener::bind(&socket_path)?;
    // Owner-only, matching the runtime's contract for this boundary. Same
    // user is inside the trust domain anyway; the probe mirrors the mode
    // because it costs one call.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let (tx, events) = mpsc::channel();
    let decision = Arc::new(Mutex::new(Decision::NoOpinion));
    let decision_for_loop = decision.clone();
    let endpoint = socket_path.to_string_lossy().into_owned();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let at = Instant::now();
            let mut raw = String::new();
            // One JSON line per connection. A read error means the hook
            // process died mid-send; `Ok(0)` means it connected and closed
            // without sending anything. Neither is a hook event, so skip the
            // connection rather than forward a phantom `unparseable` payload
            // into the stream the assertions scan.
            match std::io::BufReader::new(&mut stream).read_line(&mut raw) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            let current = *decision_for_loop.lock().unwrap();
            let _ = stream.write_all(reply_for(current).as_bytes());
            if tx.send(event_from(&raw, at)).is_err() {
                return; // the probe is done listening
            }
        }
    });
    Ok(HookListener {
        events,
        endpoint,
        decision,
    })
}

/// How long a named-pipe server task waits for its client to read the reply
/// and hang up before giving up and closing the handle anyway.
#[cfg(windows)]
const REPLY_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the named-pipe listener gets to claim its pipe name and signal
/// readiness before the probe treats it as wedged.
#[cfg(windows)]
const LISTENER_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(windows)]
pub fn start_listener(_workdir: &Path, session_tag: &str) -> std::io::Result<HookListener> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ServerOptions;

    let endpoint = format!("\\\\.\\pipe\\agent-bridge-interactive-probe-{session_tag}");
    let (tx, events) = mpsc::channel();
    let decision = Arc::new(Mutex::new(Decision::NoOpinion));
    let decision_for_loop = decision.clone();
    let pipe_name = endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                return;
            }
        };
        runtime.block_on(async move {
            // First instance claims the name exclusively so a stale
            // listener from a previous run fails loudly here instead of
            // stealing this session's payloads.
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .in_buffer_size(65_536)
                .create(&pipe_name);
            let mut server = match first {
                Ok(server) => {
                    let _ = ready_tx.send(Ok(()));
                    server
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            loop {
                if server.connect().await.is_err() {
                    continue;
                }
                // Pre-create the next instance *before* serving this
                // connection, so a hook firing concurrently (parallel tool
                // calls do happen) always finds a listener.
                let next = match ServerOptions::new()
                    .in_buffer_size(65_536)
                    .create(&pipe_name)
                {
                    Ok(next) => next,
                    Err(err) => {
                        // Without a spare instance no further hook can
                        // connect, and every later step would blame a missing
                        // hook rather than a missing listener. Say so loudly;
                        // returning drops the runtime, which is the only
                        // honest thing left to do.
                        eprintln!(
                            "interactive-probe: the hook listener could not create another pipe instance and is shutting down: {err}"
                        );
                        return;
                    }
                };
                let mut conn = std::mem::replace(&mut server, next);
                let tx = tx.clone();
                let decision = decision_for_loop.clone();
                tokio::spawn(async move {
                    let at = Instant::now();
                    let mut raw = Vec::new();
                    // Loop reads until the newline terminator: named-pipe
                    // reads arrive capped per read, so one read is not one
                    // message.
                    let mut buf = [0u8; 4096];
                    loop {
                        match conn.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                raw.extend_from_slice(&buf[..n]);
                                if raw.contains(&b'\n') {
                                    break;
                                }
                            }
                            Err(err) => {
                                // Dropping the payload silently would surface
                                // later as "the approved tool never executed".
                                eprintln!(
                                    "interactive-probe: a hook payload was lost reading the pipe: {err}"
                                );
                                return;
                            }
                        }
                    }
                    // A client that connected and closed without sending is
                    // not a hook event; forwarding the empty buffer would put
                    // a phantom `unparseable` payload into the stream the
                    // assertions scan.
                    if raw.is_empty() {
                        return;
                    }
                    let line = String::from_utf8_lossy(&raw).into_owned();
                    let current = *decision.lock().unwrap();
                    let _ = conn.write_all(reply_for(current).as_bytes()).await;
                    let _ = conn.flush().await;
                    // Hand the event over before waiting on the client, so a
                    // client that never closes cannot also hide the payload.
                    let _ = tx.send(event_from(&line, at));
                    // Closing a named-pipe server handle discards whatever the
                    // client has not yet read, so the reply can vanish between
                    // the write above and this task's end. Wait for the client
                    // to read it and hang up (read → 0), bounded so a stuck
                    // client cannot pin the task forever.
                    let _ = tokio::time::timeout(REPLY_DRAIN_TIMEOUT, async {
                        while let Ok(n) = conn.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                        }
                    })
                    .await;
                });
            }
        });
    });

    // Bounded, like every other wait in this probe: a listener thread that
    // wedges before signalling readiness — building the runtime, claiming the
    // pipe name — must become a diagnosed failure, not a probe that never
    // reports anything. A thread that dies instead disconnects the channel
    // and lands in the same error.
    match ready_rx.recv_timeout(LISTENER_READY_TIMEOUT) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(std::io::Error::other(format!(
                "the named-pipe listener did not become ready within {}s",
                LISTENER_READY_TIMEOUT.as_secs()
            )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(std::io::Error::other(
                "the named-pipe listener thread died during startup",
            ));
        }
    }
    Ok(HookListener {
        events,
        endpoint,
        decision,
    })
}

/// `hook-forward` mode: executed by the hooked CLI once per hook event.
/// Reads the payload from stdin, relays it to the probe, blocks for the
/// decision, and prints the CLI-facing decision JSON for PreToolUse events.
/// Always returns exit code 0: a hook that fails must degrade to "no
/// opinion" — diagnostics go to stderr, never a non-zero exit that would
/// disturb the session under test.
pub fn hook_forward(endpoint: &str) -> i32 {
    let mut payload = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut payload) {
        eprintln!("hook-forward: reading the payload from stdin failed: {err}");
        return 0;
    }
    // Re-serialize compact so the wire really is one line even if the CLI
    // ever pretty-prints payloads.
    let value: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("hook-forward: payload is not JSON: {err}");
            return 0;
        }
    };
    let line = match serde_json::to_string(&value) {
        Ok(line) => line,
        Err(err) => {
            eprintln!("hook-forward: re-serializing the payload failed: {err}");
            return 0;
        }
    };

    let reply = match relay(endpoint, &line) {
        Ok(reply) => reply,
        Err(err) => {
            eprintln!("hook-forward: relaying to {endpoint} failed: {err}");
            return 0;
        }
    };

    let event = value
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if event == "PreToolUse" {
        let decision = reply
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        if matches!(decision, "allow" | "deny" | "ask") {
            let reason = reply
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("interactive-probe external approver");
            println!(
                "{}",
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": decision,
                        "permissionDecisionReason": reason,
                    }
                })
            );
        }
    }
    0
}

/// Send one payload line, read one reply line.
fn relay(endpoint: &str, line: &str) -> Result<serde_json::Value, String> {
    let raw = transport_round_trip(endpoint, line)?;
    serde_json::from_str(raw.trim()).map_err(|err| format!("reply is not JSON: {err}"))
}

#[cfg(unix)]
fn transport_round_trip(endpoint: &str, line: &str) -> Result<String, String> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(endpoint).map_err(|err| format!("connect: {err}"))?;
    let timeout = Some(Duration::from_secs(30));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|err| format!("send: {err}"))?;
    let mut reply = String::new();
    std::io::BufReader::new(&mut stream)
        .read_line(&mut reply)
        .map_err(|err| format!("receive: {err}"))?;
    Ok(reply)
}

#[cfg(windows)]
fn transport_round_trip(endpoint: &str, line: &str) -> Result<String, String> {
    // A named-pipe client is a plain file open; only the busy case needs
    // handling — when every server instance is mid-connection, CreateFile
    // fails with ERROR_PIPE_BUSY (231) and the documented client pattern is
    // to retry.
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut file = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint)
        {
            Ok(file) => break file,
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if Instant::now() >= deadline {
                    return Err("pipe busy for 10s".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("connect: {err}")),
        }
    };
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|err| format!("send: {err}"))?;
    file.flush().map_err(|err| format!("send flush: {err}"))?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if reply.contains(&b'\n') {
                    break;
                }
            }
            Err(err) => return Err(format!("receive: {err}")),
        }
    }
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_settings_json_registers_every_hook_against_the_forwarder() {
        let settings = settings_json("\"/probe\" hook-forward --endpoint \"/tmp/h.sock\"");
        let hooks = settings["hooks"].as_object().expect("hooks object");
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        for event in HOOK_EVENTS {
            let entry = &hooks[event][0];
            let command = entry["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.contains("hook-forward"),
                "{event} must route to hook-forward: {command}"
            );
            assert_eq!(entry["hooks"][0]["type"], "command");
        }
        // Only the tool-lifecycle hooks carry a matcher and a timeout: the
        // CLI blocks on their reply, and an empty matcher means every tool.
        for event in TOOL_HOOKS {
            assert_eq!(hooks[event][0]["matcher"], "");
            assert_eq!(
                hooks[event][0]["hooks"][0]["timeout"],
                TOOL_HOOK_TIMEOUT_SECS
            );
        }
        assert!(hooks["SessionStart"][0].get("matcher").is_none());
        assert!(hooks["Stop"][0]["hooks"][0].get("timeout").is_none());
    }

    #[test]
    fn both_tool_lifecycle_hooks_are_registered() {
        // The deny assertion is "PreToolUse fired, PostToolUse did not";
        // dropping either one silently turns that check into a tautology.
        assert!(HOOK_EVENTS.contains(&"PreToolUse"));
        assert!(HOOK_EVENTS.contains(&"PostToolUse"));
    }

    #[test]
    fn hook_command_quotes_both_paths() {
        let command = hook_command(Path::new("/opt/probe dir/probe"), "/tmp/x/hook.sock");
        assert_eq!(
            command,
            "\"/opt/probe dir/probe\" hook-forward --endpoint \"/tmp/x/hook.sock\""
        );
    }

    #[test]
    fn reply_carries_the_current_decision() {
        let reply: serde_json::Value =
            serde_json::from_str(reply_for(Decision::Deny).trim()).unwrap();
        assert_eq!(reply["decision"], "deny");
        assert!(reply["reason"].as_str().unwrap().contains("deny"));
    }

    #[test]
    fn events_are_named_by_their_payload() {
        let event = event_from(
            "{\"hook_event_name\": \"SessionStart\", \"transcript_path\": \"/t.jsonl\"}",
            Instant::now(),
        );
        assert_eq!(event.name, "SessionStart");
        assert_eq!(event.payload["transcript_path"], "/t.jsonl");
    }

    #[test]
    fn malformed_payloads_surface_as_events_not_crashes() {
        let event = event_from("not json at all", Instant::now());
        assert_eq!(event.name, "unparseable");
        assert_eq!(event.payload["raw"], "not json at all");
    }

    #[cfg(unix)]
    #[test]
    fn unix_round_trip_delivers_payload_and_decision() {
        let dir = std::env::temp_dir().join(format!("ab-hooks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = start_listener(&dir, "test").unwrap();
        listener.set_decision(Decision::Allow);

        let reply = relay(
            listener.endpoint(),
            "{\"hook_event_name\": \"PreToolUse\", \"tool_name\": \"Bash\"}",
        )
        .unwrap();
        assert_eq!(reply["decision"], "allow");

        let event = listener
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("the listener must forward the payload");
        assert_eq!(event.name, "PreToolUse");
        assert_eq!(event.payload["tool_name"], "Bash");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_connection_that_sends_nothing_produces_no_event() {
        use std::os::unix::net::UnixStream;

        let dir = std::env::temp_dir().join(format!("ab-hooks-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = start_listener(&dir, "test").unwrap();

        // Connect and close without sending a byte — a hook process that died
        // between connect and write. This must not become a phantom event.
        UnixStream::connect(listener.endpoint()).unwrap();
        assert!(
            listener
                .events
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "an empty connection must not forward an event"
        );

        // The listener is still live: a real payload after the empty
        // connection still arrives.
        relay(
            listener.endpoint(),
            "{\"hook_event_name\": \"SessionStart\"}",
        )
        .unwrap();
        let event = listener
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a real payload after an empty connection must still arrive");
        assert_eq!(event.name, "SessionStart");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
