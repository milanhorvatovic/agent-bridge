//! The registry's contract: serialized creates under the caps, lookup
//! across live and retained-closed sessions, and the retention reaper.
//!
//! These are component tests against real sessions — each create spawns a
//! real terminal with a real child, never a mock: the platform's own shell,
//! which
//! is the smallest process guaranteed present that stays alive until told
//! otherwise. The retention test runs on real time with a shortened,
//! configurable window rather than a mocked clock: the reaper's deadlines
//! live on the runtime clock, but the sessions it retains hold real
//! terminals whose blocking I/O a paused clock would race (auto-advance
//! fires timeouts while a real reader is still draining), so shrinking the
//! window is the honest way to test the same code path.

use std::sync::Arc;
use std::time::Duration;

use agent_bridge_core::{
    AdapterSeam, BusConfig, BusError, CreateOptions, EventBus, EventFilter, RegistryConfig,
    RegistryError, SessionEntry, SessionRegistry,
};
use agent_bridge_session::{LaunchSpec, SessionConfig, SessionState, ShutdownHint, SubscriberId};

/// A real interactive child that stays alive until terminated: the
/// platform shell.
struct ShellAdapter;

fn shell() -> &'static str {
    if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }
}

impl AdapterSeam for ShellAdapter {
    fn launch_spec(&self, _options: &CreateOptions) -> LaunchSpec {
        let mut launch = LaunchSpec::new(shell());
        // A hint the caller's own geometry outranks; see the merge test.
        launch.dimensions = Some((81, 25));
        launch
    }

    fn shutdown_hint(&self) -> ShutdownHint {
        // Undeliverable on purpose: registry scenarios close by force, and
        // a graceful close would still end through escalation.
        ShutdownHint::CloseStdin
    }
}

/// An adapter whose binary does not exist — a session that fails at launch
/// and lands `Closed` without a caller ever holding its handle.
struct BrokenAdapter;

impl AdapterSeam for BrokenAdapter {
    fn launch_spec(&self, _options: &CreateOptions) -> LaunchSpec {
        LaunchSpec::new("agent-bridge-no-such-binary")
    }

    fn shutdown_hint(&self) -> ShutdownHint {
        ShutdownHint::CloseStdin
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-bridge-registry-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn registry(tag: &str, tweak: impl FnOnce(&mut RegistryConfig)) -> SessionRegistry {
    let bus = EventBus::new(BusConfig::default());
    let mut config = RegistryConfig::new(SessionConfig::new(scratch_dir(tag)));
    tweak(&mut config);
    let registry = SessionRegistry::new(bus, config);
    registry.register_adapter("shell", Arc::new(ShellAdapter));
    registry.register_adapter("broken", Arc::new(BrokenAdapter));
    registry
}

/// Serializes the terminal-hungry tests. The default harness runs test
/// functions in parallel, and the cap test alone holds 33 live terminals —
/// overlapped with the 16-way create stress the suite can exhaust the
/// operating system's terminal pool and fail with an allocation error that
/// has nothing to do with the registry. One at a time, they fit anywhere.
static PTY_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn close_all(registry: &SessionRegistry) {
    for handle in registry.iter_active() {
        let _ = handle.close(true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_x16_yields_distinct_ids_and_a_consistent_registry() {
    let _terminals = PTY_GATE.lock().await;
    let registry = registry("concurrent", |_| {});
    let creates = (0..16).map(|_| {
        let registry = registry.clone();
        // Bounded retry on terminal allocation only: sixteen simultaneous
        // openpty calls can collide in the platform's allocator (macOS
        // hands the losers ENXIO), which is a property of racing the OS
        // for terminals, not of the registry under test — the assertions
        // here are about serialized insertion and distinct ids, and a
        // retried create exercises them identically. Serializing
        // allocation below the spawn is a terminal-layer follow-up.
        tokio::spawn(async move {
            let mut attempt = 0;
            loop {
                attempt += 1;
                match registry.create("shell", CreateOptions::default()).await {
                    Err(RegistryError::Session(
                        agent_bridge_session::SessionError::LaunchFailed(_),
                    )) if attempt < 4 => {
                        tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                    }
                    outcome => return outcome,
                }
            }
        })
    });
    let mut ids = Vec::new();
    for create in creates.collect::<Vec<_>>() {
        let handle = create
            .await
            .expect("a create task must not panic")
            .expect("a create under the cap must succeed");
        ids.push(handle.session_id());
    }

    let mut distinct = ids.clone();
    distinct.sort_by_key(ToString::to_string);
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        16,
        "concurrent creates minted a duplicate id"
    );
    assert_eq!(registry.iter_active().len(), 16);
    for id in &ids {
        assert!(
            matches!(registry.lookup(id), Ok(SessionEntry::Live(_))),
            "{id} is not live in the registry"
        );
    }
    close_all(&registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_cap_rejects_the_33rd_create() {
    let _terminals = PTY_GATE.lock().await;
    let registry = registry("cap", |_| {});
    for _ in 0..32 {
        registry
            .create("shell", CreateOptions::default())
            .await
            .expect("creates below the hard cap must succeed");
    }
    let refusal = registry
        .create("shell", CreateOptions::default())
        .await
        .expect_err("the 33rd concurrent create must be refused");
    assert!(matches!(refusal, RegistryError::CapReached { limit: 32 }));
    assert_eq!(refusal.jsonrpc_code(), -32009);
    assert_eq!(registry.iter_active().len(), 32);
    close_all(&registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_keeps_a_closed_record_through_the_window_then_reaps() {
    let _terminals = PTY_GATE.lock().await;
    // The 120 s default shrunk to keep the test honest on real time; the
    // window and tick are the same configuration the deployment tunes.
    // The bus is held directly so the sweep can be observed on both maps:
    // retention bounds the registry record *and* the bus entry.
    let bus = EventBus::new(BusConfig::default());
    let mut config = RegistryConfig::new(SessionConfig::new(scratch_dir("retention")));
    config.retention = Duration::from_secs(2);
    config.reap_tick = Duration::from_millis(200);
    let registry = SessionRegistry::new(bus.clone(), config);
    registry.register_adapter("shell", Arc::new(ShellAdapter));

    let handle = registry
        .create("shell", CreateOptions::default())
        .await
        .expect("create must succeed");
    let id = handle.session_id();
    handle.close(true).await.expect("close must succeed");
    assert_eq!(handle.state(), SessionState::Closed);

    // Queryable through the window with the final record — immediately:
    // liveness is judged from the session's own state, never from the
    // separately scheduled retention stamp.
    match registry.lookup(&id) {
        Ok(SessionEntry::Closed(metadata)) => assert!(metadata.closed_at.is_some()),
        other => panic!("closed session did not read as a retained record: {other:?}"),
    }
    // The bus still knows the sealed id through the window.
    assert!(
        matches!(
            bus.subscribe(&id.to_string(), EventFilter::All),
            Err(BusError::Sealed(_))
        ),
        "the sealed bus entry must survive until the reap"
    );

    // Past the window, the id answers -32002, the reap is on the
    // supervisor-action counter, and the bus entry went with the record.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match registry.lookup(&id) {
            Err(refusal) => {
                assert_eq!(refusal.jsonrpc_code(), -32002);
                break;
            }
            Ok(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(_) => panic!("the retained record was never reaped"),
        }
    }
    assert_eq!(registry.cleanup_orphan_count(), 1);
    assert!(
        matches!(
            bus.subscribe(&id.to_string(), EventFilter::All),
            Err(BusError::UnknownSession(_))
        ),
        "the reap must remove the bus entry along with the record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_validates_before_it_allocates() {
    let registry = registry("validation", |_| {});

    let unknown = registry
        .create("no-such-adapter", CreateOptions::default())
        .await
        .expect_err("an unregistered adapter must refuse");
    assert!(matches!(unknown, RegistryError::AdapterNotFound(_)));
    assert_eq!(unknown.jsonrpc_code(), -32001);

    let oversized = registry
        .create(
            "shell",
            CreateOptions {
                dimensions: Some((65_535, 65_535)),
                creator: None,
            },
        )
        .await
        .expect_err("a 63 GiB grid request must refuse");
    assert_eq!(oversized.jsonrpc_code(), -32602);
    assert!(
        registry.iter_active().is_empty(),
        "a refused create left an entry behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_launch_reports_32005_and_leaves_no_record_behind() {
    let _terminals = PTY_GATE.lock().await;
    // The window shrunk so the reaper gets several chances during the
    // test: a zero cleanup count after those ticks proves create removed
    // the record itself, rather than the reaper quietly retiring a
    // leftover.
    let registry = registry("broken", |config| {
        config.retention = Duration::from_millis(200);
        config.reap_tick = Duration::from_millis(100);
    });
    let refusal = registry
        .create("broken", CreateOptions::default())
        .await
        .expect_err("a nonexistent binary must fail the create");
    assert_eq!(refusal.jsonrpc_code(), -32005);
    assert!(registry.iter_active().is_empty());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        registry.cleanup_orphan_count(),
        0,
        "a failed launch must not leave a retained record for the reaper"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retained_cap_evicts_the_oldest_closed_record_first() {
    let _terminals = PTY_GATE.lock().await;
    // Retention stays at its long default so nothing here is the reaper's
    // doing: only the count bound can remove a record inside this test.
    let bus = EventBus::new(BusConfig::default());
    let mut config = RegistryConfig::new(SessionConfig::new(scratch_dir("retained-cap")));
    config.max_retained = 1;
    let registry = SessionRegistry::new(bus.clone(), config);
    registry.register_adapter("shell", Arc::new(ShellAdapter));

    let first = registry
        .create("shell", CreateOptions::default())
        .await
        .expect("create must succeed");
    let first_id = first.session_id();
    first.close(true).await.expect("close must succeed");
    // The retention stamp rides a separately scheduled watcher task; the
    // pause dwarfs a task poll so the first stamp lands before the second
    // session exists, keeping "oldest" unambiguous.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let second = registry
        .create("shell", CreateOptions::default())
        .await
        .expect("create must succeed");
    let second_id = second.session_id();
    second.close(true).await.expect("close must succeed");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match registry.lookup(&first_id) {
            Err(refusal) => {
                assert_eq!(refusal.jsonrpc_code(), -32002);
                break;
            }
            Ok(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(_) => panic!("the over-cap record was never evicted"),
        }
    }
    assert!(
        matches!(registry.lookup(&second_id), Ok(SessionEntry::Closed(_))),
        "the newest retained record must survive the eviction"
    );
    assert!(
        matches!(
            bus.subscribe(&first_id.to_string(), EventFilter::All),
            Err(BusError::UnknownSession(_))
        ),
        "the eviction must remove the bus entry along with the record"
    );
    assert_eq!(registry.cleanup_orphan_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_launch_never_evicts_a_retained_record() {
    let _terminals = PTY_GATE.lock().await;
    // The retained set is full at one record when the broken create runs.
    // If the failing launch were stamped as retained-closed before its
    // entry is removed, the cap enforcement would evict the valid record;
    // the watcher starting only on a known launch outcome is what this
    // pins.
    let mut config = RegistryConfig::new(SessionConfig::new(scratch_dir("no-evict")));
    config.max_retained = 1;
    let bus = EventBus::new(BusConfig::default());
    let registry = SessionRegistry::new(bus, config);
    registry.register_adapter("shell", Arc::new(ShellAdapter));
    registry.register_adapter("broken", Arc::new(BrokenAdapter));

    let kept = registry
        .create("shell", CreateOptions::default())
        .await
        .expect("create must succeed");
    let kept_id = kept.session_id();
    kept.close(true).await.expect("close must succeed");
    // Let the watcher stamp the record before the broken create runs.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let refusal = registry
        .create("broken", CreateOptions::default())
        .await
        .expect_err("a nonexistent binary must fail the create");
    assert_eq!(refusal.jsonrpc_code(), -32005);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        matches!(registry.lookup(&kept_id), Ok(SessionEntry::Closed(_))),
        "the failed launch evicted a valid retained record"
    );
    assert_eq!(registry.cleanup_orphan_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_geometry_outranks_the_adapter_hint_and_creator_owns_the_write_side() {
    let _terminals = PTY_GATE.lock().await;
    let registry = registry("options", |_| {});
    let handle = registry
        .create(
            "shell",
            CreateOptions {
                dimensions: Some((100, 30)),
                creator: Some(SubscriberId("peer-0".to_string())),
            },
        )
        .await
        .expect("create must succeed");
    let dimensions = handle.metadata().dimensions;
    assert_eq!((dimensions.cols, dimensions.rows), (100, 30));
    assert_eq!(
        handle.writer().map(|writer| writer.0),
        Some("peer-0".to_string())
    );
    handle.close(true).await.expect("close must succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_of_an_unknown_id_is_32002() {
    let registry = registry("lookup", |_| {});
    let refusal = registry
        .lookup(&agent_bridge_session::SessionId::new())
        .expect_err("a never-issued id must refuse");
    assert_eq!(refusal.jsonrpc_code(), -32002);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sessions_events_reach_bus_subscribers_and_the_stream_ends_at_close() {
    let _terminals = PTY_GATE.lock().await;
    // The seam the registry exists to wire: what the actor publishes
    // arrives at a bus subscriber with gap-free seqs, and the close path's
    // seal ends the subscriber's stream after the closed event.
    let bus = EventBus::new(BusConfig::default());
    let registry = SessionRegistry::new(
        bus.clone(),
        RegistryConfig::new(SessionConfig::new(scratch_dir("bus"))),
    );
    registry.register_adapter("shell", Arc::new(ShellAdapter));

    let handle = registry
        .create("shell", CreateOptions::default())
        .await
        .expect("create must succeed");
    // Attach with backfill from 0: the create-flow events were published
    // before this subscriber existed, and the replay seam is exactly what
    // a late attacher uses to still see the whole ladder.
    let (mut subscription, _plan) = bus
        .subscribe_from(&handle.session_id().to_string(), Some(0), EventFilter::All)
        .expect("the session must be subscribable");

    handle.close(true).await.expect("close must succeed");

    let mut seen = Vec::new();
    let mut expected_seq = 0;
    loop {
        let received = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
            .await
            .expect("the sealed stream must end");
        let Some(event) = received else { break };
        assert_eq!(event.seq, expected_seq, "a gap in the delivered stream");
        expected_seq += 1;
        seen.push(event.kind.event_type().to_owned());
    }
    assert_eq!(
        seen.first().map(String::as_str),
        Some("lifecycle.session.created")
    );
    assert_eq!(
        seen.last().map(String::as_str),
        Some("lifecycle.session.closed")
    );
}
