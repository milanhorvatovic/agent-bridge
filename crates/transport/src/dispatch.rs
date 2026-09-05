//! The method dispatcher: a request in, a response out, and — for `attach` —
//! a subscription task that turns bus events into `session.event`
//! notifications.
//!
//! The dispatcher knows the runtime only through the core's façade: it asks
//! the registry to create, look up, drain, and close sessions, and the bus to
//! subscribe, and it never reaches into a session's internals. Every typed
//! error a call can raise is mapped to the protocol table in [`crate::error`],
//! so a code is chosen beside the variant that raised it, never reconstructed
//! from a message here.

use std::collections::HashSet;
use std::time::Duration;

use agent_bridge_core::{
    ApprovalDecision, ApprovalId, DisconnectReason, EventBus, EventFilter, SessionEntry,
    SessionError, SessionHandle, SessionId, SessionRegistry, Subscription,
};
use agent_bridge_events::EventKind;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::error::{JsonRpcError, from_bus, from_registry, from_session};
use crate::method::{self, WireDecision};
use crate::notify::{self, EofReason, event_frame};
use crate::outbound::Outbound;
use crate::rpc::{Request, Response};

/// The runtime surface the dispatcher drives: the session registry, the event
/// bus its subscriptions read, and the static facts `runtime.info` reports.
/// Cheap to clone — the registry and bus are handles — so the dispatcher owns
/// one and the binary keeps its own.
#[derive(Clone)]
pub struct RuntimeContext {
    /// Session lifecycle: create, look up, drain, close.
    pub registry: SessionRegistry,
    /// The event bus a `session.attach` subscribes against.
    pub bus: EventBus,
    /// What `runtime.info` returns.
    pub info: RuntimeInfoRef,
}

/// The static facts `runtime.info` reports, assembled by the binary at
/// startup. A borrowed, cheaply-cloned record rather than recomputed per call
/// — the adapter set and version do not change over a runtime's life.
#[derive(Clone)]
pub struct RuntimeInfoRef {
    /// The runtime binary's version.
    pub version: String,
    /// The adapters registered at startup, by name.
    pub adapters: Vec<String>,
    /// Capability tags the runtime advertises.
    pub capabilities: Vec<String>,
    /// The event taxonomy's `schema_version`, the sole version `runtime.info`
    /// carries on the wire.
    pub schema_version: u32,
}

/// Turns requests into responses, and owns the attach subscription tasks it
/// spawns so they can be ended at drain.
pub struct Dispatcher {
    ctx: RuntimeContext,
    outbound: Outbound,
    /// Flipped by `runtime.shutdown` after its response is queued; the serve
    /// loop watches the same channel and begins the drain.
    shutdown: watch::Sender<bool>,
    /// A subscription an `attach` created but whose forwarder is not yet
    /// spawned. The serve loop enqueues the attach acknowledgement first, then
    /// spawns the forwarder ([`Dispatcher::spawn_pending_attach`]), so no
    /// `session.event` can precede its own `attach` response on the wire. The
    /// subscription is live from the moment it was created, so events in the
    /// gap queue rather than being lost.
    pending_attach: Option<(String, Subscription)>,
    /// The `session.attach` forwarder tasks. A [`JoinSet`](tokio::task::JoinSet)
    /// rather than a plain vector for two reasons: finished forwarders are
    /// reaped as new ones are spawned, so a long-lived peer that creates,
    /// attaches, and closes many sessions does not accumulate dead handles; and
    /// at drain [`JoinSet::shutdown`] aborts *and awaits* every task, so their
    /// outbound handles are provably dropped before the writer is reclaimed.
    /// Each forwarder yields its session id when it ends, so reaping it also
    /// clears that id from `attached` — a re-attach after a close or a lag
    /// disconnect then creates a fresh subscription rather than reading as
    /// idempotent and delivering nothing.
    subscriptions: tokio::task::JoinSet<String>,
    /// The sessions this connection has an *active* forwarder for. A second
    /// `attach` for one is idempotent — the same acknowledgement, no new
    /// subscription — so a peer cannot spawn an unbounded fan of forwarders,
    /// each duplicating every event, against a bus that caps no subscriber. An
    /// id is removed when its forwarder ends (reaped from `subscriptions`), so
    /// a re-attach after a close or a lag disconnect is a real new attachment,
    /// not a silent no-op.
    attached: HashSet<String>,
}

impl Dispatcher {
    /// A dispatcher over `ctx`, writing through `outbound`, signalling
    /// operator shutdown on `shutdown`.
    pub fn new(ctx: RuntimeContext, outbound: Outbound, shutdown: watch::Sender<bool>) -> Self {
        Self {
            ctx,
            outbound,
            shutdown,
            pending_attach: None,
            subscriptions: tokio::task::JoinSet::new(),
            attached: HashSet::new(),
        }
    }

    /// Dispatch one frame, returning the response to write back, or `None` when
    /// there is nothing to answer.
    ///
    /// A request (a call carrying an id) is answered against that id. A
    /// *notification* — a call with no id — still runs its method; JSON-RPC only
    /// forbids *answering* it, so a fire-and-forget `runtime.shutdown` or
    /// `session.send` takes effect while its response is suppressed. A frame
    /// that could not be parsed at all is still answered, against `null` per the
    /// spec, because a parse or invalid-request error has no id to blame.
    pub async fn dispatch(&mut self, frame: Bytes) -> Option<Response> {
        let request = match Request::parse(&frame) {
            Ok(request) => request,
            Err(rejection) => return Some(Response::error(rejection.id, rejection.error)),
        };
        // The method name is attacker-influenced; bound its length before it is
        // looked up, the same reason the frame body is bounded before it is
        // read. An over-long name is method-not-found for a request; a
        // notification carrying one is dropped, since it cannot be answered.
        if request.method.len() > method::MAX_METHOD_NAME_BYTES {
            return request.id.clone().map(|id| {
                Response::error(
                    id,
                    JsonRpcError::method_not_found("<name exceeds the method-name cap>"),
                )
            });
        }
        // Run the method regardless of id; answer only when one was carried.
        let outcome = self.route(&request).await;
        request.id.clone().map(|id| match outcome {
            Ok(result) => Response::result(id, result),
            Err(error) => Response::error(id, error),
        })
    }

    /// The method table: the one match over method names, each arm a handler.
    async fn route(&mut self, request: &Request) -> Result<Value, JsonRpcError> {
        match request.method.as_str() {
            method::RUNTIME_INFO => {
                expect_no_params(request)?;
                Ok(self.runtime_info())
            }
            method::RUNTIME_SHUTDOWN => {
                expect_no_params(request)?;
                Ok(self.runtime_shutdown())
            }
            method::SESSION_CREATE => self.session_create(request).await,
            method::SESSION_ATTACH => self.session_attach(request),
            method::SESSION_SEND => self.session_send(request).await,
            method::SESSION_RESOLVE_APPROVAL => self.session_resolve_approval(request).await,
            method::SESSION_INTERRUPT => self.session_interrupt(request).await,
            method::SESSION_RESIZE => self.session_resize(request).await,
            method::SESSION_CLOSE => self.session_close(request).await,
            other => Err(JsonRpcError::method_not_found(other)),
        }
    }

    fn runtime_info(&self) -> Value {
        let info = &self.ctx.info;
        json!({
            "version": info.version,
            "adapters": info.adapters,
            "capabilities": info.capabilities,
            "schema_version": info.schema_version,
        })
    }

    /// Signal the serve loop to drain and exit. The response is returned
    /// normally and queued first; the loop observes the flip on its next turn,
    /// so the caller receives its acknowledgement before the drain begins.
    fn runtime_shutdown(&self) -> Value {
        // A receiver always exists — the serve loop holds one — so the send
        // cannot fail; if it somehow did, the loop would still end on the
        // stdin EOF that follows a client closing its side.
        let _ = self.shutdown.send(true);
        json!({ "ok": true })
    }

    async fn session_create(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionCreateParams = parse_params(request)?;
        let options = agent_bridge_core::CreateOptions {
            dimensions: params.dims.map(|[cols, rows]| (cols, rows)),
            creator: None,
        };
        match self.ctx.registry.create(&params.adapter, options).await {
            Ok(handle) => Ok(json!({ "session_id": handle.session_id().to_string() })),
            Err(error) => Err(from_registry(&error)),
        }
    }

    fn session_attach(&mut self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionAttachParams = parse_params(request)?;
        if params.from_seq.is_some() {
            return Err(JsonRpcError::invalid_params(
                "from_seq backfill is unsupported until the Phase-3 attach surface; \
                 attach without it to subscribe at head",
            ));
        }
        // A caller that pins the event schema learns before consuming a single
        // event whether it is talking to a runtime whose taxonomy it
        // understands — the point of the check being on attach.
        if let Some(expected) = params.expected_schema_version {
            let actual = self.ctx.info.schema_version;
            if expected != actual {
                return Err(JsonRpcError::schema_version_mismatch(expected, actual));
            }
        }
        // Resolve to a live session first so a wrong or closed id answers with
        // the session-facing code rather than a bus error the client cannot
        // read as clearly.
        let handle = self.resolve_live(&params.session_id)?;
        let session_id = handle.session_id().to_string();
        // Clear any ended attachments first, so the idempotence check below sees
        // only live ones — a re-attach after a lag disconnect must make a fresh
        // subscription, not read as idempotent and deliver nothing.
        self.reap_ended_attachments();
        // One subscription per session per connection: a repeat attach returns
        // the same acknowledgement without creating a second subscription, so a
        // peer cannot fan out duplicate forwarders and exhaust the writer.
        if self.attached.contains(&session_id) {
            return Ok(json!({ "session_id": session_id }));
        }
        let subscription = self
            .ctx
            .bus
            .subscribe(&session_id, EventFilter::All)
            .map_err(|error| from_bus(&error))?;
        // Recorded only once the subscription exists, so a failed subscribe
        // leaves a later retry free to attach rather than falsely idempotent.
        self.attached.insert(session_id.clone());
        // Hold the subscription for the serve loop to spawn *after* it enqueues
        // this acknowledgement, so the ack always precedes the first
        // `session.event`. The subscription is already live, so events arriving
        // in the gap queue rather than being lost.
        self.pending_attach = Some((session_id.clone(), subscription));
        Ok(json!({ "session_id": session_id }))
    }

    async fn session_send(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionSendParams = parse_params(request)?;
        let handle = self.resolve_live(&params.session_id)?;
        handle
            .send(Bytes::from(params.input.into_bytes()))
            .await
            .map(|()| empty())
            .map_err(|error| from_session(&error))
    }

    async fn session_resolve_approval(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionResolveApprovalParams = parse_params(request)?;
        // A reason explains a denial; supplied with allow or ask it would be
        // silently dropped, so the strict-parameter contract refuses it rather
        // than let a caller believe an unused reason took effect.
        if params.reason.is_some() && params.decision != WireDecision::Deny {
            return Err(JsonRpcError::invalid_params(
                "reason may only accompany a deny decision",
            ));
        }
        let handle = self.resolve_live(&params.session_id)?;
        let decision = match params.decision {
            WireDecision::Allow => ApprovalDecision::Allow,
            WireDecision::Deny => ApprovalDecision::Deny {
                reason: params.reason,
            },
            WireDecision::Ask => ApprovalDecision::Ask,
        };
        handle
            .resolve_approval(ApprovalId(params.approval_id), decision)
            .await
            .map(|()| empty())
            .map_err(|error| from_session(&error))
    }

    async fn session_interrupt(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionInterruptParams = parse_params(request)?;
        let handle = self.resolve_live(&params.session_id)?;
        handle
            .interrupt()
            .await
            .map(|()| empty())
            .map_err(|error| from_session(&error))
    }

    async fn session_resize(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionResizeParams = parse_params(request)?;
        let handle = self.resolve_live(&params.session_id)?;
        handle
            .resize(params.dims[0], params.dims[1])
            .await
            .map(|()| empty())
            .map_err(|error| from_session(&error))
    }

    async fn session_close(&self, request: &Request) -> Result<Value, JsonRpcError> {
        let params: method::SessionCloseParams = parse_params(request)?;
        let id = parse_session_id(&params.session_id)?;
        match self.ctx.registry.lookup(&id) {
            // A live session is closed on request.
            Ok(SessionEntry::Live(handle)) => handle
                .close(params.force)
                .await
                .map(|()| empty())
                .map_err(|error| from_session(&error)),
            // Closing an already-closed session is a resolved race, not an
            // error — the same idempotence the session layer's close path
            // gives a second close.
            Ok(SessionEntry::Closed(_)) => Ok(empty()),
            Err(error) => Err(from_registry(&error)),
        }
    }

    /// Resolve a wire `session_id` to a live handle, or the wire error a
    /// caller should see: an unparseable id is invalid params, a well-formed
    /// but unknown id is session-not-found, and a closed one is session-closed.
    fn resolve_live(&self, session_id: &str) -> Result<SessionHandle, JsonRpcError> {
        let id = parse_session_id(session_id)?;
        match self.ctx.registry.lookup(&id) {
            Ok(SessionEntry::Live(handle)) => Ok(handle),
            Ok(SessionEntry::Closed(_)) => Err(from_session(&SessionError::SessionClosed)),
            Err(error) => Err(from_registry(&error)),
        }
    }

    /// Spawn the forwarder for a subscription the serve loop has already
    /// acknowledged. Kept separate from [`Dispatcher::session_attach`] so the
    /// ack is enqueued before the first `session.event`.
    pub fn spawn_pending_attach(&mut self) {
        if let Some((session_id, subscription)) = self.pending_attach.take() {
            self.spawn_attach(session_id, subscription);
        }
    }

    /// Spawn the task that drains one subscription onto the wire: each bus
    /// event as a `session.event`, then — when the stream ends — the
    /// `session.eof` naming why. The task holds an outbound handle, which is why
    /// the drain awaits these before reclaiming the writer.
    fn spawn_attach(&mut self, session_id: String, mut subscription: Subscription) {
        self.reap_ended_attachments();
        let outbound = self.outbound.clone();
        self.subscriptions.spawn(async move {
            // The child's exit code rides the `session_closed` eof; it is
            // echoed from the `lifecycle.session.closed` event as it passes,
            // so the eof carries the value the design's eof contract names.
            let mut exit_code = None;
            let mut sealed = false;
            while let Some(event) = subscription.recv().await {
                if let EventKind::LifecycleSessionClosed(payload) = &event.kind {
                    exit_code = payload.exit_code;
                }
                if outbound.send(event_frame(&event)).is_err() {
                    // The writer sealed (die-loudly): nothing more can go out.
                    sealed = true;
                    break;
                }
            }
            if !sealed {
                // The `subscriber_lagging` eof reason names the lag; its
                // authoritative payload — the events lost and the queue bounds
                // that explain the disconnect — is emitted just before the eof
                // as a `transport.error` notification (no event envelope, so no
                // seq that could move the subscriber's stream backward; the
                // subscription is named in the payload's detail), rather than
                // folded into the sequenced event stream.
                let reason = match subscription.disconnect_reason() {
                    Some(DisconnectReason::Lagging) => {
                        if let Some(payload) = subscription.disconnect_error() {
                            let _ = outbound
                                .send(notify::subscription_error_frame(&session_id, payload));
                        }
                        EofReason::SubscriberLagging
                    }
                    // A seal that could not hand over every accepted event
                    // reports the shortfall on the eof rather than dropping it
                    // silently — the bus counted it, so the wire names it.
                    None => EofReason::SessionClosed {
                        exit_code,
                        events_lost: subscription.undelivered_at_seal(),
                    },
                };
                let _ = outbound.send(notify::eof_frame(&session_id, reason));
            }
            // Yielded so reaping this forwarder clears the id from `attached`.
            session_id
        });
    }

    /// Reap forwarders that have finished, dropping each ended session id from
    /// `attached` so the set names only live attachments. A long-lived peer's
    /// session churn does not pile up completed handles, and — the reason the
    /// id is returned — a re-attach after a close or a lag disconnect is not
    /// mistaken for a live one and answered idempotently with no subscription.
    fn reap_ended_attachments(&mut self) {
        while let Some(joined) = self.subscriptions.try_join_next() {
            if let Ok(session_id) = joined {
                self.attached.remove(&session_id);
            }
        }
    }

    /// Close every live session, bounded by `grace`. Graceful closes run in
    /// parallel — the natural default; a session's own close path is
    /// self-bounding — and any that outlast the grace are forced, so the
    /// drain itself cannot outrun its window on a wedged session.
    pub async fn drain(&self, grace: Duration) {
        let active = self.ctx.registry.iter_active();
        if active.is_empty() {
            return;
        }
        let mut set = tokio::task::JoinSet::new();
        for handle in active {
            set.spawn(async move {
                // A session mid-launch rejects close — graceful and forced
                // alike — until it settles, so treating that immediate
                // `InvalidStateForOperation` as done would let the drain
                // report clean while a `Launching` session is still active,
                // leaving the registry's abandonment guard to close it after
                // the runtime may already have reported a clean exit. Retry
                // through the launch refusal exactly as that guard does,
                // bounded by the outer grace; a session still launching when
                // the grace expires is beyond this drain and left to that
                // guard, which the grace-exceeded warning names.
                while let Err(SessionError::InvalidStateForOperation { .. }) =
                    handle.close(false).await
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
        let join_all = async { while set.join_next().await.is_some() {} };
        if tokio::time::timeout(grace, join_all).await.is_err() {
            tracing::warn!("session drain exceeded its grace window; forcing the remainder");
            // Force the remainder concurrently, as the graceful phase does: a
            // forced close still waits out bounded termination and cleanup, so
            // running them serially would make shutdown latency scale with the
            // session count.
            let mut forced = tokio::task::JoinSet::new();
            for handle in self.ctx.registry.iter_active() {
                forced.spawn(async move {
                    let _ = handle.close(true).await;
                });
            }
            while forced.join_next().await.is_some() {}
        }
    }

    /// Wind down every attach task, then confirm it is gone so the outbound
    /// handles it held are dropped — the precondition for reclaiming the writer
    /// to flush its tail.
    ///
    /// When `flush` is set, each forwarder is given a bounded chance to finish
    /// on its own — the preceding drain has sealed every session, so each
    /// reaches the end of its subscription, delivers its session's final events
    /// and the closing `session.eof`, and returns. This is the operator-shutdown
    /// path, where truncating an attached subscriber's stream would drop the
    /// very `closed`/`eof` it was waiting for. When `flush` is clear — a
    /// protocol close or a dead wire, where the peer will read nothing more —
    /// the forwarders are ended at once without emitting trailing frames.
    ///
    /// Either way the set is finally [`shut down`](tokio::task::JoinSet::shutdown),
    /// which aborts every remaining task *and awaits it*, so all outbound handles
    /// are provably dropped before the writer is reclaimed for its tail flush.
    pub async fn end_subscriptions(&mut self, flush: bool) {
        if flush {
            let deadline = tokio::time::Instant::now() + SUBSCRIPTION_DRAIN_GRACE;
            while !self.subscriptions.is_empty() {
                match tokio::time::timeout_at(deadline, self.subscriptions.join_next()).await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(
                            "an attach subscription did not flush within its grace; aborting it"
                        );
                        break;
                    }
                }
            }
        }
        self.subscriptions.shutdown().await;
    }
}

/// How long the attach forwarders get to flush their sessions' final events
/// and `session.eof` at shutdown before any straggler is aborted. Generous
/// against the near-instant real case — a sealed subscription ends as soon as
/// its queue drains, and the outbound enqueue never blocks — and a backstop,
/// not a normal path.
const SUBSCRIPTION_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// An empty JSON object — the result of a verb that succeeds with nothing to
/// report.
fn empty() -> Value {
    json!({})
}

/// Refuse parameters for a method that takes none. Absent, `null`, an empty
/// object, and an empty array are the accepted spellings of "no params" — the
/// two structured forms JSON-RPC allows, by-name and by-position; anything else
/// is invalid params, so the parameterless methods hold the same strict line
/// the typed ones do rather than silently ignoring a field a caller sent.
fn expect_no_params(request: &Request) -> Result<(), JsonRpcError> {
    match &request.params {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) if map.is_empty() => Ok(()),
        // An empty array is by-position what an empty object is by-name: zero
        // parameters. A client that encodes every call's params positionally
        // sends `[]` here, and refusing it while accepting `{}` would answer
        // its `runtime.info` with `-32602`. A non-empty array is still refused.
        Some(Value::Array(items)) if items.is_empty() => Ok(()),
        Some(_) => Err(JsonRpcError::invalid_params(
            "this method takes no parameters",
        )),
    }
}

/// Parse a wire `session_id`, mapping a malformed one to invalid params. A
/// well-formed id that names no session is *not* caught here — that is a
/// lookup's `session_not_found`, a different and more specific answer.
fn parse_session_id(session_id: &str) -> Result<SessionId, JsonRpcError> {
    session_id
        .parse()
        .map_err(|_| JsonRpcError::invalid_params("session_id is not a valid session id"))
}

/// Deserialize a request's params into a typed, `deny_unknown_fields` shape.
/// Absent params deserialize as JSON null, which every param-bearing method's
/// struct rejects — the invalid-params answer a call with no params deserves.
fn parse_params<T: DeserializeOwned>(request: &Request) -> Result<T, JsonRpcError> {
    // Deserialize the params in place rather than cloning them first. A
    // near-limit frame — the cap reaches 1 GiB — would otherwise hold the
    // framed body, the request's parsed params, and a full clone of them at
    // once, turning the wire's size bound into several times that in peak
    // memory. `&Value` is itself a `Deserializer`, so the owned `T` is built
    // without the copy.
    let null = Value::Null;
    let params = request.params.as_ref().unwrap_or(&null);
    T::deserialize(params)
        .map_err(|error| JsonRpcError::invalid_params(format!("invalid params: {error}")))
}
