//! The lag policy held to its published contract, through the public
//! surface only: the grace window's two endpoints (a burst survives, a
//! sustained non-drain disconnects), the out-of-band `transport.error`
//! payload a disconnect leaves beside the ended stream, lag isolation
//! between subscribers, the bounded memory of a stalled queue, and the
//! staged-dispatch properties — a re-entrant waker cannot deadlock the
//! bus, and a panicking one cannot wedge it.
//!
//! Timing assertions run on `tokio::time::pause` virtual time — the
//! plan's CI-flake mitigation: precision lives here where the clock is
//! mocked, while the real-clock latency comparison uses a generous
//! tolerance.

use std::sync::Arc;
use std::time::Duration;

use agent_bridge_core::{
    BackpressureConfig, BusConfig, DisconnectReason, EventBus, EventFilter, Subscription,
};
use agent_bridge_events::{Event, EventBody, EventKind, StreamToken, TransportErrorCode};

fn token(content: &str) -> EventBody {
    EventBody::new(EventKind::StreamToken(StreamToken {
        source: None,
        content: content.into(),
    }))
}

fn bus(queue_bound: usize, grace: Duration) -> EventBus {
    EventBus::new(BusConfig {
        backpressure: BackpressureConfig { queue_bound, grace },
        ..BusConfig::default()
    })
}

/// The disconnect payload's fixed shape: `transport.error` with code
/// `subscriber_lagging`, carrying the loss count in its detail — beside
/// the stream, never in it.
fn assert_lagging(subscription: &Subscription) -> u64 {
    assert_eq!(
        subscription.disconnect_reason(),
        Some(DisconnectReason::Lagging)
    );
    let payload = subscription
        .disconnect_error()
        .expect("a lag disconnect carries its payload");
    assert_eq!(payload.code, TransportErrorCode::SubscriberLagging);
    payload
        .detail
        .get("events_lost")
        .and_then(serde_json::Value::as_u64)
        .expect("the payload states what was lost")
}

/// No stream ever carries a synthesized event: everything received is
/// canonical history.
fn assert_all_canonical(events: &[Arc<Event>]) {
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.kind, EventKind::TransportError(_))),
        "a synthesized terminal event leaked into the stream"
    );
}

/// Everything left on the stream, in order, to its end.
async fn drain_to_end(subscription: &mut Subscription) -> Vec<Arc<Event>> {
    let mut events = Vec::new();
    while let Some(event) = subscription.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test(start_paused = true)]
async fn lag_disconnect_respects_grace_window() {
    let bus = bus(2, Duration::from_secs(2));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    // Queue (2) fills, one event parks, the rest are counted lost — all
    // at t=0, so the grace deadline sits at t=2s.
    for i in 0..10 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    // Just short of the deadline, with the sweep having ticked repeatedly:
    // no disconnect before grace.
    tokio::time::sleep(Duration::from_millis(1_900)).await;
    assert_eq!(stalled.disconnect_reason(), None, "disconnected early");
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 0);

    // Past it: promptly disconnected (the sweep tick bounds the delay).
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(stalled.disconnect_reason(), Some(DisconnectReason::Lagging));

    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), 2, "the two queued events, then the end");
    assert_eq!((events[0].seq, events[1].seq), (0, 1));
    assert_all_canonical(&events);
    let lost = assert_lagging(&stalled);
    assert_eq!(lost, 8, "ten published, two queued: eight lost, stated");
    assert_eq!(
        stalled.undelivered_at_seal(),
        None,
        "a lag disconnect is reported as one, not as a seal-time shortfall"
    );
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn burst_within_grace_survives() {
    let bus = bus(2, Duration::from_secs(2));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    // Fill the queue and park one — a burst, not a stall.
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    // Draining within grace flushes the overflow in order and cancels the
    // deadline; the episode leaves no residue.
    assert_eq!(subscription.recv().await.unwrap().seq, 0);
    publisher.publish(token("3")).unwrap();
    assert_eq!(subscription.recv().await.unwrap().seq, 1);
    assert_eq!(subscription.recv().await.unwrap().seq, 2);
    publisher.publish(token("4")).unwrap();
    assert_eq!(subscription.recv().await.unwrap().seq, 3);
    assert_eq!(subscription.recv().await.unwrap().seq, 4);

    // Far past any grace window: a survived burst stays survived.
    tokio::time::sleep(Duration::from_secs(10)).await;
    publisher.publish(token("5")).unwrap();
    assert_eq!(subscription.recv().await.unwrap().seq, 5);
    assert_eq!(subscription.disconnect_reason(), None);
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 0);

    bus.seal_session("s").unwrap();
    assert!(subscription.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn caught_up_subscriber_gets_its_parked_event_and_survives() {
    let bus = bus(2, Duration::from_millis(500));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    // A burst ends with one event parked — and then the stream goes quiet.
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    // The subscriber catches up fully. With no further publish to carry
    // it, the parked event must still arrive (the sweep flushes it once
    // room exists), and the drained subscriber must never be sealed by
    // the idle deadline — it drained within grace; the bus just had not
    // handed the last event over yet.
    assert_eq!(subscription.recv().await.unwrap().seq, 0);
    assert_eq!(subscription.recv().await.unwrap().seq, 1);
    assert_eq!(subscription.recv().await.unwrap().seq, 2);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(subscription.disconnect_reason(), None);
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 0);

    bus.seal_session("s").unwrap();
    assert!(subscription.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn seal_flushes_a_parked_event_when_room_exists() {
    let bus = bus(2, Duration::from_secs(2));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    // Queue [0, 1], event 2 parked — then the subscriber makes room and
    // the session closes. The parked event was an accepted publish; the
    // close path hands it over rather than dropping it with the slot.
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    assert_eq!(subscription.recv().await.unwrap().seq, 0);
    bus.seal_session("s").unwrap();

    let events = drain_to_end(&mut subscription).await;
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2],
        "the parked event rides the close-path flush"
    );
    assert_eq!(subscription.disconnect_reason(), None);
    assert_eq!(subscription.disconnect_error(), None);
    assert_eq!(
        subscription.undelivered_at_seal(),
        None,
        "a seal that handed everything over reports no shortfall"
    );
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn seal_with_no_room_announces_the_loss_without_a_lag_verdict() {
    let bus = bus(2, Duration::from_secs(2));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    // Queue full and one event parked with no room to flush: the session
    // ending is not a lag disconnect — no verdict — but the loss is
    // announced to the subscriber itself, never merely logged away.
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    bus.seal_session("s").unwrap();

    let events = drain_to_end(&mut subscription).await;
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(subscription.disconnect_reason(), None);
    assert_eq!(
        subscription.undelivered_at_seal(),
        Some(1),
        "the accepted event the seal could not hand over is announced"
    );
    assert_eq!(
        subscription.disconnect_error(),
        None,
        "a shortfall at close must not borrow the lag disconnect's code"
    );
    assert_eq!(
        bus.metrics().disconnect_subscriber_count(),
        0,
        "a session seal is not a bus-initiated disconnect"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_stream_lag_still_resolves() {
    let bus = bus(2, Duration::from_millis(500));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    // Fill, park, lose — then the stream goes quiet: no publish will ever
    // observe this deadline, so only the coarse sweep can.
    for i in 0..4 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(stalled.disconnect_reason(), Some(DisconnectReason::Lagging));
    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), 2);
    assert_all_canonical(&events);
    assert_eq!(assert_lagging(&stalled), 2, "the parked event and one more");
}

#[tokio::test(start_paused = true)]
async fn seal_ordering_terminal_is_last() {
    let bus = bus(1, Duration::from_millis(100));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    // Let the sweep seal at the deadline, then race further triggers at
    // it: later publishes find no slot — the seal *is* the removal, which
    // is what makes it idempotent.
    tokio::time::sleep(Duration::from_millis(400)).await;
    for i in 3..6 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), 1, "the one queued event, then the end");
    assert_all_canonical(&events);
    assert_lagging(&stalled);
    assert_eq!(
        bus.metrics().disconnect_subscriber_count(),
        1,
        "racing triggers seal once: the seal is the removal"
    );
}

#[tokio::test(start_paused = true)]
async fn disconnect_error_is_set_whatever_the_filter_admits() {
    let bus = bus(1, Duration::from_millis(100));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus
        .subscribe("s", EventFilter::Exact("stream.token".into()))
        .unwrap();

    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The subscription only ever asked for tokens, but why its stream
    // ended rides beside the stream, untouched by the filter.
    let events = drain_to_end(&mut stalled).await;
    assert_all_canonical(&events);
    assert_lagging(&stalled);
}

#[tokio::test(start_paused = true)]
async fn lag_isolation_healthy_gets_everything() {
    const ROUNDS: u64 = 50;
    const PER_ROUND: u64 = 4;

    let bus = bus(4, Duration::from_millis(200));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut healthy = bus.subscribe("s", EventFilter::All).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    let mut received = Vec::new();
    for round in 0..ROUNDS {
        for i in 0..PER_ROUND {
            publisher
                .publish(token(&(round * PER_ROUND + i).to_string()))
                .unwrap();
        }
        for _ in 0..PER_ROUND {
            received.push(healthy.recv().await.unwrap().seq);
        }
        if round == 10 {
            // Cross the stalled subscriber's grace deadline mid-run; the
            // healthy one must not notice.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    bus.seal_session("s").unwrap();

    // The healthy subscriber saw every event, in order, before, during,
    // and after its sibling's disconnect.
    let expected: Vec<u64> = (0..ROUNDS * PER_ROUND).collect();
    assert_eq!(received, expected);
    assert_eq!(healthy.disconnect_reason(), None);
    assert!(healthy.recv().await.is_none());

    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), 4, "the four queued events, then the end");
    assert_all_canonical(&events);
    assert_lagging(&stalled);
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn stalled_memory_bounded() {
    const BOUND: usize = 8;
    const PUBLISHED: u64 = 100;

    let bus = bus(BOUND, Duration::from_millis(200));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..PUBLISHED {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    // What a stalled subscriber holds is the queue bound and nothing else
    // that survives to be delivered — everything past the bound and the
    // one-slot overflow was dropped and counted.
    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), BOUND);
    let lost = assert_lagging(&stalled);
    assert_eq!(u64::try_from(BOUND).unwrap() + lost, PUBLISHED);
}

#[test]
#[should_panic(expected = "backpressure.grace")]
fn an_unrepresentable_grace_is_refused_at_construction() {
    // A deployment typo (seconds parsed from an absurd number) must fail
    // where it can be fixed, not as an overflow panic on the synchronous
    // publish path when the first overflow event arms its deadline.
    let _ = EventBus::new(BusConfig {
        backpressure: BackpressureConfig {
            queue_bound: 8,
            grace: Duration::MAX,
        },
        ..BusConfig::default()
    });
}

#[test]
fn config_defaults_match_the_design_table() {
    let config = BackpressureConfig::default();
    assert_eq!(config.queue_bound, 1024);
    assert_eq!(config.grace, Duration::from_secs(2));
    let bus_config = BusConfig::default();
    assert_eq!(bus_config.backpressure.queue_bound, 1024);
    assert_eq!(bus_config.backpressure.grace, Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn non_default_config_is_respected() {
    let bus = bus(3, Duration::from_millis(300));
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..10 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    // Inside the non-default grace window: alive.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stalled.disconnect_reason(), None);
    // Past it (and far under the 2 s default, which would still be
    // waiting): the next publish observes the expiry.
    tokio::time::sleep(Duration::from_millis(250)).await;
    publisher.publish(token("trigger")).unwrap();
    assert_eq!(stalled.disconnect_reason(), Some(DisconnectReason::Lagging));

    // The non-default bound decided what was kept.
    let events = drain_to_end(&mut stalled).await;
    assert_eq!(events.len(), 3);
    assert_all_canonical(&events);
}

#[tokio::test(start_paused = true)]
async fn grace_stays_unarmed_while_replay_drains() {
    let bus = bus(2, Duration::from_millis(300));
    let publisher = bus.register_session("s".into()).unwrap();

    // History for the backfill to preload.
    for i in 0..5 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    let (mut subscription, _plan) = bus.subscribe_from("s", Some(0), EventFilter::All).unwrap();

    // Live events pile up while the replay buffer sits undrained: the
    // queue fills and overflows, but the grace window must not run — a
    // subscriber catching up on instruction is not lagging.
    for i in 5..9 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(subscription.disconnect_reason(), None);

    // Draining the replay arms the clock at the next policy touch...
    for i in 0..5 {
        assert_eq!(subscription.recv().await.unwrap().seq, i);
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ...and an undrained live queue past grace now seals normally.
    assert_eq!(
        subscription.disconnect_reason(),
        Some(DisconnectReason::Lagging)
    );
    let events = drain_to_end(&mut subscription).await;
    assert_eq!(events.len(), 2);
    assert_eq!((events[0].seq, events[1].seq), (5, 6));
    assert_lagging(&subscription);
}

#[tokio::test(start_paused = true)]
async fn disconnect_counter_increments_per_lag_seal() {
    let bus = bus(1, Duration::from_millis(100));
    let publisher = bus.register_session("s".into()).unwrap();
    let stalled_a = bus.subscribe("s", EventFilter::All).unwrap();
    let stalled_b = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(bus.metrics().disconnect_subscriber_count(), 2);
    drop(stalled_a);
    drop(stalled_b);
    // Voluntary unsubscribes are not disconnects.
    assert_eq!(bus.metrics().disconnect_subscriber_count(), 2);
}

/// The staged-dispatch regression pair. A hand-rolled waker that re-enters the bus
/// inline from `wake()` — publish, subscribe, seal — must complete without
/// deadlock, because every wake-producing send happens outside the bus
/// locks and a re-entrant publish simply stages for the drainer already
/// running.
#[test]
fn reentrant_waker_cannot_deadlock_the_bus() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    struct Reenter {
        bus: EventBus,
        publisher: Arc<agent_bridge_core::Publisher>,
        hits: AtomicUsize,
    }

    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            if self.hits.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            // Inline, mid-delivery: stage another event behind the active
            // drainer, attach and drop a subscriber, and seal the session.
            self.publisher.publish(token("reentrant")).unwrap();
            drop(self.bus.subscribe("s", EventFilter::All).unwrap());
            self.bus.seal_session("s").unwrap();
        }
    }

    let bus = EventBus::new(BusConfig::default());
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    let reenter = Arc::new(Reenter {
        bus: bus.clone(),
        publisher: Arc::clone(&publisher),
        hits: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&reenter));
    {
        let mut context = Context::from_waker(&waker);
        let mut pending = std::pin::pin!(subscription.recv());
        assert!(pending.as_mut().poll(&mut context).is_pending());
    }

    // This send fires the waker inline; the wake's own publish, subscribe,
    // and seal must all return. A deadlock hangs the test right here.
    publisher.publish(token("outer")).unwrap();
    assert!(
        reenter.hits.load(Ordering::SeqCst) > 0,
        "the waker never ran"
    );

    // The seal raced the drain and lands only after the drain runs dry:
    // the re-entrant publish returned Ok mid-drain, so its event must be
    // delivered before the stream ends — a seal that dropped staged
    // events would silently lose exactly the session's last ones.
    assert_eq!(
        poll_ready_recv(&mut subscription)
            .expect("the outer event")
            .seq,
        0
    );
    assert_eq!(
        poll_ready_recv(&mut subscription)
            .expect("the event staged by the waker survives the racing seal")
            .seq,
        1
    );
    assert!(
        poll_ready_recv(&mut subscription).is_none(),
        "after the staged tail, the sealed stream ends"
    );

    // The channel is sealed, the bus is not wedged, and other sessions
    // work.
    assert_eq!(
        publisher.publish(token("late")),
        Err(agent_bridge_core::BusError::Sealed("s".into()))
    );
    let fresh = bus.register_session("s2".into()).unwrap();
    let mut observer = bus.subscribe("s2", EventFilter::All).unwrap();
    fresh.publish(token("alive")).unwrap();
    let received = poll_ready_recv(&mut observer);
    assert_eq!(received.expect("the bus still delivers").seq, 0);
}

/// A waker that attaches a subscriber, stages an event behind the active
/// drain, and then blows up — the worst unwind the guard has to survive.
/// Shared by the two panic-recovery tests below.
struct StagingBomb {
    bus: EventBus,
    publisher: Arc<agent_bridge_core::Publisher>,
    rescued: std::sync::Mutex<Option<Subscription>>,
}

impl std::task::Wake for StagingBomb {
    fn wake(self: Arc<Self>) {
        *self.rescued.lock().unwrap() = Some(self.bus.subscribe("s", EventFilter::All).unwrap());
        self.publisher.publish(token("staged-by-bomb")).unwrap();
        panic!("waker bomb");
    }
}

fn arm_bomb(bus: &EventBus, publisher: &Arc<agent_bridge_core::Publisher>) -> Arc<StagingBomb> {
    Arc::new(StagingBomb {
        bus: bus.clone(),
        publisher: Arc::clone(publisher),
        rescued: std::sync::Mutex::new(None),
    })
}

/// The other half of the staged-dispatch pair: a waker that panics unwinds
/// through the drain, and the panic guard resets the drainer flag — the
/// channel keeps working instead of staging into a queue nobody will ever
/// drain. The event the bomb staged before blowing up is not lost and not
/// reordered: the next publish adopts the backlog ahead of its own newer
/// event, so a subscriber attached during the failed drain still observes
/// strict `seq` order.
#[test]
fn a_panicking_waker_cannot_wedge_the_channel() {
    use std::task::{Context, Waker};

    let bus = EventBus::new(BusConfig::default());
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());
    let mut doomed = bus.subscribe("s", EventFilter::All).unwrap();

    let bomb = arm_bomb(&bus, &publisher);
    let waker = Waker::from(Arc::clone(&bomb));
    {
        let mut context = Context::from_waker(&waker);
        let mut pending = std::pin::pin!(doomed.recv());
        assert!(pending.as_mut().poll(&mut context).is_pending());
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        publisher.publish(token("boom")).unwrap();
    }));
    assert!(result.is_err(), "the bomb must actually have gone off");

    // The unwound drain dropped the slot it held: the doomed stream got
    // its delivered event and then ended.
    assert_eq!(
        poll_ready_recv(&mut doomed)
            .expect("delivered pre-bomb")
            .seq,
        0
    );
    assert!(poll_ready_recv(&mut doomed).is_none());

    // The next publish finds the orphaned backlog and lines its own event
    // up behind it: the bomb's subscriber — attached mid-drain, entitled
    // to everything from seq 1 — sees 1 then 2, never 2 then 1.
    publisher.publish(token("after")).unwrap();
    let mut rescued = bomb
        .rescued
        .lock()
        .unwrap()
        .take()
        .expect("the bomb subscribed before it went off");
    assert_eq!(
        poll_ready_recv(&mut rescued)
            .expect("the orphaned staged event is adopted first")
            .seq,
        1
    );
    assert_eq!(
        poll_ready_recv(&mut rescued)
            .expect("then the newer event")
            .seq,
        2
    );
    drop(doomed);
}

/// The same orphaned backlog with no follow-up publish at all: only the
/// coarse sweep can adopt it, and must — an event whose publish returned
/// `Ok` stays deliverable even when its drainer died and the stream then
/// went quiet.
#[tokio::test(start_paused = true)]
async fn sweep_rescues_a_backlog_orphaned_by_a_panicking_drain() {
    use std::task::{Context, Waker};

    let bus = EventBus::new(BusConfig::default());
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());
    let mut doomed = bus.subscribe("s", EventFilter::All).unwrap();

    let bomb = arm_bomb(&bus, &publisher);
    let waker = Waker::from(Arc::clone(&bomb));
    {
        let mut context = Context::from_waker(&waker);
        let mut pending = std::pin::pin!(doomed.recv());
        assert!(pending.as_mut().poll(&mut context).is_pending());
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        publisher.publish(token("boom")).unwrap();
    }));
    assert!(result.is_err(), "the bomb must actually have gone off");

    let mut rescued = bomb
        .rescued
        .lock()
        .unwrap()
        .take()
        .expect("the bomb subscribed before it went off");
    let event = rescued
        .recv()
        .await
        .expect("the sweep adopts the orphaned backlog within a tick");
    assert_eq!(event.seq, 1);
    drop(doomed);
}

/// The sweeper flushes parked events, which sends, which fires a
/// subscriber's waker — so a panicking waker can unwind out of the one
/// task that watches every channel's idle-stream deadlines. It must take
/// only its own subscription with it: another session's lag, observable by
/// nothing but the sweep, still has to resolve afterwards.
#[tokio::test(start_paused = true)]
async fn a_panicking_waker_cannot_kill_the_sweeper() {
    use std::task::{Context, Wake, Waker};

    struct Bomb;
    impl Wake for Bomb {
        fn wake(self: Arc<Self>) {
            panic!("waker bomb");
        }
    }

    let bus = bus(1, Duration::from_millis(500));
    let doomed_publisher = bus.register_session("doomed".into()).unwrap();
    let mut doomed = bus.subscribe("doomed", EventFilter::All).unwrap();
    let other_publisher = bus.register_session("other".into()).unwrap();
    let other = bus.subscribe("other", EventFilter::All).unwrap();

    // Queue one event and park the next, then drain the queue so the
    // sweep's flush has room — and leave a panicking waker registered.
    doomed_publisher.publish(token("queued")).unwrap();
    doomed_publisher.publish(token("parked")).unwrap();
    assert_eq!(doomed.recv().await.unwrap().seq, 0);
    let waker = Waker::from(Arc::new(Bomb));
    {
        let mut context = Context::from_waker(&waker);
        let mut pending = std::pin::pin!(doomed.recv());
        assert!(pending.as_mut().poll(&mut context).is_pending());
    }

    // The next tick flushes the parked event into that waker and blows up
    // inside the sweeper task.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // A second session now lags with no further publishes to observe it:
    // only a live sweeper can disconnect it.
    other_publisher.publish(token("queued")).unwrap();
    other_publisher.publish(token("parked")).unwrap();
    other_publisher.publish(token("lost")).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        other.disconnect_reason(),
        Some(DisconnectReason::Lagging),
        "the sweeper died with the panicking subscription and left every other \
         session's idle lag unwatched"
    );
}

/// `recv` without an async runtime, for the two manual-waker tests: the
/// queue is non-empty by construction, so a single no-op-waker poll
/// completes immediately.
fn poll_ready_recv(subscription: &mut Subscription) -> Option<Arc<Event>> {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(subscription.recv());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(event) => event,
        Poll::Pending => panic!("a non-empty queue answered Pending"),
    }
}

/// The producer-never-blocks acceptance, as a statistical comparison: the
/// publish path under a fully-stalled subscriber against the
/// no-subscriber baseline. Generous tolerance — this is the real clock on
/// a shared CI runner; the mocked-clock tests carry the precision.
#[test]
fn publish_latency_stalled_vs_baseline() {
    use std::time::Instant;

    const WARMUP: usize = 2_000;
    const SAMPLES: usize = 20_000;

    fn median_publish_ns(with_stalled_subscriber: bool) -> u64 {
        // An hour of grace keeps the deadline unexpired for the whole
        // measurement: what is measured is the steady lossy state, not
        // the one-time seal.
        let bus = bus(64, Duration::from_secs(3_600));
        let publisher = bus.register_session("bench".into()).unwrap();
        let _stalled =
            with_stalled_subscriber.then(|| bus.subscribe("bench", EventFilter::All).unwrap());
        for _ in 0..WARMUP {
            publisher.publish(token("warmup")).unwrap();
        }
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let body = token("measured");
            let start = Instant::now();
            publisher.publish(body).unwrap();
            samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    let baseline = median_publish_ns(false);
    let stalled = median_publish_ns(true);
    assert!(
        stalled <= baseline * 5 + 2_000,
        "publish under a stalled subscriber (median {stalled} ns) diverged from the \
         no-subscriber baseline (median {baseline} ns)"
    );
}
