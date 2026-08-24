//! The session registry: who exists, under what cap, and for how long
//! after closing.
//!
//! One registry per runtime. Insertion is serialized, so concurrent
//! `session.create` calls mint distinct ids against a consistent map, the
//! caps are enforced where the map is (soft 8 warns, hard 32 refuses), and
//! a closed session's record outlives its actor by the retention window
//! (120 s, `runtime.closed_session_retention_seconds`) so a late reader
//! can still fetch final metadata and the exit code before the reaper
//! turns the id into `-32002`.
//!
//! The registry is also where the bus and a session meet: create registers
//! the session on the bus, hands the actor its one [`Publisher`] behind the
//! session crate's sink seam, and the actor's close path seals it. The
//! adapter side is deliberately thin — [`AdapterSeam`] carries exactly the
//! two things the create flow reads (`LaunchSpec`, `ShutdownHint`) and is
//! replaced when the `Adapter` trait freezes in its own change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_bridge_events::EventBody;
use agent_bridge_session::{
    EventSink, LaunchSpec, SessionConfig, SessionError, SessionHandle, SessionId, SessionMetadata,
    SessionSpec, SessionState, ShutdownHint, SinkSealed, SubscriberId, spawn_session,
    validate_dimensions,
};
use tokio::time::{Instant, MissedTickBehavior};

use crate::bus::{EventBus, Publisher};

/// How the registry is tuned. Populated from configuration by the wiring
/// layer; this crate knows the defaults.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Concurrent-session count past which create logs a warning. A
    /// guardrail, not a license enforcer — each real CLI session costs its
    /// operator quota the runtime cannot see.
    pub soft_cap: usize,
    /// Concurrent-session count at which create is refused (`-32009`).
    pub hard_cap: usize,
    /// How long a closed session's record stays queryable
    /// (`runtime.closed_session_retention_seconds`).
    pub retention: Duration,
    /// The reaper's tick. Coarse; anything at or under a quarter of the
    /// retention window keeps the overshoot honest.
    pub reap_tick: Duration,
    /// Retained-closed records past which the oldest are evicted before
    /// their window expires. The retention window bounds a record's life
    /// in time; this bounds the map in count, so churn faster than the
    /// window drains cannot grow the registry without limit.
    pub max_retained: usize,
    /// Tuning handed to each session at create.
    pub session: SessionConfig,
}

impl RegistryConfig {
    /// The contract defaults around the given per-session tuning.
    pub fn new(session: SessionConfig) -> Self {
        Self {
            soft_cap: 8,
            hard_cap: 32,
            retention: Duration::from_secs(120),
            reap_tick: Duration::from_secs(15),
            max_retained: 128,
            session,
        }
    }
}

/// What a caller may vary per `session.create`.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Requested terminal geometry as `(cols, rows)`. Outranks the
    /// adapter's hint; validated against the session bound before anything
    /// allocates.
    pub dimensions: Option<(u16, u16)>,
    /// The creating peer, recorded as the session's initial writer owner
    /// (state only in v1).
    pub creator: Option<SubscriberId>,
}

/// The pre-freeze adapter seam: exactly what the create flow consumes.
///
/// The real `Adapter` trait freezes in `adapter-api` in its own change;
/// until then this narrow stand-in keeps the registry's shape honest
/// without guessing at the frozen surface. Implementations are registered by name via
/// [`SessionRegistry::register_adapter`].
pub trait AdapterSeam: Send + Sync {
    /// How to launch this adapter's CLI, given the caller's options.
    fn launch_spec(&self, options: &CreateOptions) -> LaunchSpec;
    /// How this CLI prefers to be asked to exit.
    fn shutdown_hint(&self) -> ShutdownHint;
}

/// What the registry can refuse or fail at.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No adapter is registered under this name.
    #[error("adapter {0:?} is not registered")]
    AdapterNotFound(String),
    /// The id names no live session and no retained closed one — wrong,
    /// or already reaped.
    #[error("session {0} not found")]
    SessionNotFound(SessionId),
    /// The hard cap is holding the line.
    #[error("session cap reached: {limit} concurrent sessions")]
    CapReached {
        /// The configured hard cap.
        limit: usize,
    },
    /// The session layer refused or failed — invalid geometry, launch
    /// failure, a closed session.
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl RegistryError {
    /// The JSON-RPC error code the transport reports.
    pub fn jsonrpc_code(&self) -> i32 {
        match self {
            RegistryError::AdapterNotFound(_) => -32001,
            RegistryError::SessionNotFound(_) => -32002,
            RegistryError::CapReached { .. } => -32009,
            RegistryError::Session(error) => error.jsonrpc_code(),
        }
    }
}

/// What a lookup finds under an id.
//
// The derive is safe here and stays safe: a handle prints shape and
// identity by its own hand-written `Debug`, and metadata is counters and
// timestamps — neither reaches session content.
#[derive(Debug)]
pub enum SessionEntry {
    /// The session is live; here is its control surface.
    Live(SessionHandle),
    /// The session ended within the retention window; its final record is
    /// still readable.
    Closed(SessionMetadata),
}

struct Entry {
    handle: SessionHandle,
    /// Set when the session's actor ends; the reaper measures retention
    /// from here.
    closed_at: Option<Instant>,
}

/// Whether an entry still holds a live session. Two signals, both needed:
/// the session's own state covers the scheduling gap before the close
/// watcher has stamped anything, and the stamp covers an actor that died
/// without reaching `Closed` — whose state watch will report a live-looking
/// state forever. Either saying "over" means over.
fn is_live(entry: &Entry) -> bool {
    entry.closed_at.is_none() && entry.handle.state() != SessionState::Closed
}

/// The runtime's one session registry. Cheap to clone; clones see one map.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    bus: EventBus,
    config: RegistryConfig,
    adapters: Mutex<HashMap<String, Arc<dyn AdapterSeam>>>,
    sessions: Mutex<HashMap<SessionId, Entry>>,
    /// Reaps performed — the `cleanup_orphan` supervisor-action counter
    /// the Phase-3 `runtime.health` surface windows into.
    cleanup_orphans: AtomicU64,
}

/// A poisoned lock means a holder panicked mid-update; nothing here can
/// continue meaningfully against that, and unwrapping loudly beats limping.
fn lock<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
    mutex.lock().expect("a registry lock holder panicked")
}

impl SessionRegistry {
    /// A new registry over `bus`, with its reaper running.
    ///
    /// Must be called within a tokio runtime — the reaper is a task, and
    /// it lives and dies with the registry (a dropped registry ends it on
    /// its next tick).
    ///
    /// # Panics
    ///
    /// When `config.reap_tick` is zero. The refusal is here, at the
    /// construction site, because the alternative is worse than a panic: a
    /// zero interval panics *inside* the detached reaper task, leaving a
    /// registry that constructs fine and then never reclaims anything.
    pub fn new(bus: EventBus, config: RegistryConfig) -> Self {
        assert!(
            !config.reap_tick.is_zero(),
            "reap_tick must be nonzero: the reaper's interval cannot fire on a zero period"
        );
        let inner = Arc::new(RegistryInner {
            bus,
            config,
            adapters: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            cleanup_orphans: AtomicU64::new(0),
        });
        spawn_reaper(&inner);
        Self { inner }
    }

    /// Register an adapter under `name`. A second registration replaces
    /// the first — the wiring layer owns the set, and it registers each
    /// adapter once at startup.
    pub fn register_adapter(&self, name: impl Into<String>, adapter: Arc<dyn AdapterSeam>) {
        lock(&self.inner.adapters).insert(name.into(), adapter);
    }

    /// Create a session, standing its whole stack up.
    ///
    /// Validates the adapter and geometry, serializes the id mint and
    /// insertion (concurrent creates get distinct ids against a consistent
    /// map), registers the session on the bus, spawns its actor, and then
    /// awaits the launch outcome — `Ok` once the session stands at
    /// `Connecting`, or the typed failure after the state machine has
    /// already walked `Launching → Closed` with its paired error events.
    /// A failed launch leaves no record behind: its id was never returned,
    /// so a retained entry would be unreachable bookkeeping — the error in
    /// hand is the whole story, and the entry and its bus registration are
    /// removed before create reports it.
    pub async fn create(
        &self,
        adapter: &str,
        options: CreateOptions,
    ) -> Result<SessionHandle, RegistryError> {
        let seam = lock(&self.inner.adapters)
            .get(adapter)
            .cloned()
            .ok_or_else(|| RegistryError::AdapterNotFound(adapter.to_string()))?;

        // Parameter validation before anything allocates:
        // the caller's geometry outranks the adapter's hint, and whichever
        // wins must fit the session bound.
        let mut launch = seam.launch_spec(&options);
        if options.dimensions.is_some() {
            launch.dimensions = options.dimensions;
        }
        if let Some((cols, rows)) = launch.dimensions {
            validate_dimensions(cols, rows).map_err(RegistryError::Session)?;
        }
        let shutdown_hint = seam.shutdown_hint();

        // The critical section is bookkeeping only — cap check, id mint,
        // bus registration, task spawn, insertion. Nothing inside it
        // touches the disk: even the session's log opens on the actor's
        // side of the seam, so a stalled filesystem cannot freeze the
        // registry surface for unrelated sessions.
        let (handle, launch_outcome) = {
            let mut sessions = lock(&self.inner.sessions);
            let live = sessions.values().filter(|entry| is_live(entry)).count();
            if live >= self.inner.config.hard_cap {
                return Err(RegistryError::CapReached {
                    limit: self.inner.config.hard_cap,
                });
            }
            if live + 1 > self.inner.config.soft_cap {
                tracing::warn!(
                    live = live + 1,
                    soft_cap = self.inner.config.soft_cap,
                    hard_cap = self.inner.config.hard_cap,
                    "concurrent sessions passed the soft cap"
                );
            }

            let session_id = SessionId::new();
            let publisher = self
                .inner
                .bus
                .register_session(session_id.to_string())
                .expect("a freshly minted UUIDv4 cannot already be registered on the bus");
            let sink = BusSink {
                publisher,
                bus: self.inner.bus.clone(),
                session_id: session_id.to_string(),
            };

            let spawned = match spawn_session(
                SessionSpec {
                    session_id,
                    adapter: adapter.to_string(),
                    launch,
                    shutdown_hint,
                    creator: options.creator.clone(),
                    config: self.inner.config.session.clone(),
                },
                Box::new(sink),
            ) {
                Ok(spawned) => spawned,
                Err(error) => {
                    // The bus entry must not outlive a session that never
                    // started; sealing is how a session id ends.
                    let _ = self.inner.bus.seal_session(&session_id.to_string());
                    return Err(RegistryError::Session(error));
                }
            };

            sessions.insert(
                session_id,
                Entry {
                    handle: spawned.handle.clone(),
                    closed_at: None,
                },
            );
            (spawned.handle, spawned.launch)
        };

        // The close watcher starts only once the launch outcome is known.
        // Started at insert, it could stamp a failing launch as a
        // retained-closed record in the gap before the error path removes
        // it — and a stamped corpse counts against the retained cap, so a
        // broken create could evict a valid record a reader was entitled
        // to. Liveness needs no watcher in the gap: `is_live` reads the
        // session's own state.

        match launch_outcome.await {
            Ok(Ok(())) => {
                watch_for_close(&self.inner, &handle);
                Ok(handle)
            }
            Ok(Err(error)) => {
                // The actor walked its failure route and sealed before
                // reporting, so both halves of the registration can go
                // now: the id was never handed out, and a record nobody
                // can name is not retention, it is a leak on the
                // retention clock.
                lock(&self.inner.sessions).remove(&handle.session_id());
                if !self
                    .inner
                    .bus
                    .forget_sealed(&handle.session_id().to_string())
                {
                    tracing::error!(
                        session_id = %handle.session_id(),
                        "a failed launch's bus entry was not sealed"
                    );
                }
                Err(RegistryError::Session(error))
            }
            // The actor ended without reporting — nothing to hand out.
            // The entry stays, and the watcher is still started: this
            // ending skipped the seal, so the watcher's backstop supplies
            // it (wait_closed returns at once on a dead actor) and the
            // reaper retires the record on the retention clock, the same
            // path as any other abnormal death.
            Err(_) => {
                watch_for_close(&self.inner, &handle);
                Err(RegistryError::Session(SessionError::SessionClosed))
            }
        }
    }

    /// Find a session by id: live, or closed-but-retained. A reaped or
    /// never-issued id is [`RegistryError::SessionNotFound`] — the
    /// `-32002` the wire reports.
    ///
    /// Live-or-closed is judged from the session's own state, never from
    /// the retention bookkeeping: `closed_at` is stamped by a separately
    /// scheduled task, and a verdict read from it would count a session
    /// as alive for the scheduling gap after its close resolved — a
    /// spurious `CapReached` at the boundary, and a `Live` answer for a
    /// corpse.
    pub fn lookup(&self, id: &SessionId) -> Result<SessionEntry, RegistryError> {
        let sessions = lock(&self.inner.sessions);
        match sessions.get(id) {
            Some(entry) if is_live(entry) => Ok(SessionEntry::Live(entry.handle.clone())),
            Some(entry) => Ok(SessionEntry::Closed(entry.handle.metadata())),
            None => Err(RegistryError::SessionNotFound(*id)),
        }
    }

    /// Every live session, for supervision and shutdown drain.
    pub fn iter_active(&self) -> Vec<SessionHandle> {
        lock(&self.inner.sessions)
            .values()
            .filter(|entry| is_live(entry))
            .map(|entry| entry.handle.clone())
            .collect()
    }

    /// How many retained records the reaper has cleaned up — the
    /// `cleanup_orphan` supervisor-action counter.
    pub fn cleanup_orphan_count(&self) -> u64 {
        self.inner.cleanup_orphans.load(Ordering::Relaxed)
    }
}

/// Mark the entry closed the moment its actor ends, so retention runs on
/// the session's clock rather than on anyone remembering to ask — and
/// backstop the one ending the actor cannot announce for itself.
fn watch_for_close(inner: &Arc<RegistryInner>, handle: &SessionHandle) {
    let registry = Arc::downgrade(inner);
    let handle = handle.clone();
    tokio::spawn(async move {
        let last = handle.wait_closed().await;
        let Some(inner) = registry.upgrade() else {
            return;
        };
        if last != SessionState::Closed {
            // wait_closed returned because the actor is *gone*, not
            // because it finished: a panic skipped the whole close path,
            // including the seal that ends the bus stream. Subscribers
            // must still observe an end rather than parking forever, and
            // the record must still age out — the seal is the part the
            // registry can supply from here; reclaiming whatever child
            // the dead actor abandoned is the supervisor's province.
            tracing::error!(
                session_id = %handle.session_id(),
                state = %handle.state(),
                "session actor ended without reaching Closed; sealing its stream as a backstop"
            );
            let _ = inner.bus.seal_session(&handle.session_id().to_string());
        }
        let mut sessions = lock(&inner.sessions);
        if let Some(entry) = sessions.get_mut(&handle.session_id()) {
            entry.closed_at = Some(Instant::now());
        }
        // The count bound is enforced where the count grows — under the
        // same lock as the stamp, so two sessions closing at once cannot
        // both count the map under the cap. Oldest records go first: the
        // retention window is a courtesy to late readers, and the reader
        // most likely to still come asking is the one whose session ended
        // most recently.
        let mut retained: Vec<(SessionId, Instant)> = sessions
            .iter()
            .filter_map(|(id, entry)| entry.closed_at.map(|closed_at| (*id, closed_at)))
            .collect();
        if retained.len() > inner.config.max_retained {
            retained.sort_by_key(|(_, closed_at)| *closed_at);
            let excess = retained.len() - inner.config.max_retained;
            for (session_id, _) in retained.into_iter().take(excess) {
                sessions.remove(&session_id);
                if !inner.bus.forget_sealed(&session_id.to_string()) {
                    tracing::error!(%session_id, "evicted a session whose bus entry was not sealed");
                }
                tracing::info!(%session_id, "evicted the oldest retained record over the retained cap");
            }
            inner
                .cleanup_orphans
                .fetch_add(excess as u64, Ordering::Relaxed);
        }
    });
}

/// The retention reaper: a coarse timer over the map, not a timer per
/// session — thirty-two timers guarding thirty-two records would be
/// machinery for a sweep that costs one lock.
fn spawn_reaper(inner: &Arc<RegistryInner>) {
    let registry = Arc::downgrade(inner);
    let tick_period = inner.config.reap_tick;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(tick_period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(inner) = registry.upgrade() else {
                return;
            };
            let now = Instant::now();
            let mut reaped = 0u64;
            {
                let mut sessions = lock(&inner.sessions);
                sessions.retain(|session_id, entry| {
                    let expired = entry
                        .closed_at
                        .is_some_and(|closed_at| now.duration_since(closed_at) >= inner.config.retention);
                    if expired {
                        reaped += 1;
                        // The bus entry goes with the record: the seal
                        // deliberately left the id resident pending the
                        // session layer's answer, and the retention window
                        // is that answer — past it, the id means -32002
                        // everywhere, and the bus's memory is bounded by
                        // the same clock as the registry's. A refusal here
                        // means the stream was never sealed, which the
                        // close path and the watch backstop both prevent —
                        // worth a loud record if it ever happens.
                        if !inner.bus.forget_sealed(&session_id.to_string()) {
                            tracing::error!(%session_id, "reaped a session whose bus entry was not sealed");
                        }
                        tracing::info!(%session_id, "reaped a closed session past its retention window");
                    }
                    !expired
                });
            }
            if reaped > 0 {
                inner.cleanup_orphans.fetch_add(reaped, Ordering::Relaxed);
            }
        }
    });
}

/// The bus, as the session crate's sink seam: publish through the
/// session's one stamping handle; seal through the bus, so subscribers
/// observe the end of the stream after the final event.
struct BusSink {
    publisher: Publisher,
    bus: EventBus,
    session_id: String,
}

impl EventSink for BusSink {
    fn publish(&self, body: EventBody) -> Result<u64, SinkSealed> {
        self.publisher.publish(body).map_err(|_| SinkSealed)
    }

    fn seal(&self) {
        if let Err(error) = self.bus.seal_session(&self.session_id) {
            tracing::error!(session_id = %self.session_id, %error, "sealing the session failed");
        }
    }
}
