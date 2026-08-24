//! The bus held to its published contract, through the public surface only:
//! gap-free per-session `seq` under concurrent publishing, independent
//! in-order fanout, filter semantics, session/global isolation, non-blocking
//! publish under a stalled subscriber, and the sealed end of a stream.

use std::sync::Arc;
use std::thread;

use agent_bridge_core::{
    BackpressureConfig, BusConfig, BusError, EventBus, EventFilter, Subscription,
};
use agent_bridge_events::{
    Event, EventBody, EventKind, LifecycleTurnStarted, RuntimeNotice, StreamToken, ToolCallStarted,
    ToolResult,
};

fn token(content: &str) -> EventBody {
    EventBody::new(EventKind::StreamToken(StreamToken {
        source: None,
        content: content.into(),
    }))
}

fn tool_call(call_id: &str) -> EventBody {
    EventBody::new(EventKind::ToolCallStarted(ToolCallStarted {
        call_id: call_id.into(),
        tool: "bash".into(),
        command: None,
    }))
}

fn tool_result(call_id: &str) -> EventBody {
    EventBody::new(EventKind::ToolResult(ToolResult {
        call_id: call_id.into(),
        content: String::new(),
    }))
}

fn notice(notification_type: &str) -> EventBody {
    EventBody::new(EventKind::RuntimeNotice(RuntimeNotice {
        notification_type: notification_type.into(),
        message: None,
        detail: Default::default(),
    }))
}

async fn drain(subscription: &mut Subscription, count: usize) -> Vec<Arc<Event>> {
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        events.push(
            subscription
                .recv()
                .await
                .expect("the stream ended before the expected count"),
        );
    }
    events
}

#[test]
fn seq_consecutive_from_zero_100k() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 12_500;

    let bus = EventBus::new(BusConfig::default());
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let publisher = Arc::clone(&publisher);
            thread::spawn(move || {
                (0..PER_THREAD)
                    .map(|_| publisher.publish(token("x")).unwrap())
                    .collect::<Vec<u64>>()
            })
        })
        .collect();

    let mut all = Vec::with_capacity((THREADS * PER_THREAD) as usize);
    for handle in handles {
        let seqs = handle.join().unwrap();
        // Within one thread the stamped sequence must move forward: a
        // publish that returns cannot be overtaken by the same caller.
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
        all.extend(seqs);
    }
    // Across all threads together: every value exactly once, no gaps,
    // starting at 0 — the choke point holds under contention.
    all.sort_unstable();
    let expected: Vec<u64> = (0..THREADS * PER_THREAD).collect();
    assert_eq!(all, expected);
}

#[tokio::test]
async fn two_subscribers_full_ordered_stream() {
    const COUNT: u64 = 1_000;

    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    let mut first = bus.subscribe("s", EventFilter::All).unwrap();
    let mut second = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..COUNT {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    for subscription in [&mut first, &mut second] {
        let events = drain(subscription, COUNT as usize).await;
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
            assert_eq!(event.session_id.as_deref(), Some("s"));
        }
    }
}

#[tokio::test]
async fn the_envelope_is_completed_at_publish() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    let body = EventBody::new(EventKind::LifecycleTurnStarted(LifecycleTurnStarted {}))
        .with_correlation_id("req-7");
    publisher.publish(body).unwrap();
    publisher.publish(token("second")).unwrap();

    let events = drain(&mut subscription, 2).await;
    let first = &events[0];
    assert_eq!(first.schema_version, agent_bridge_events::SCHEMA_VERSION);
    assert_eq!(first.session_id.as_deref(), Some("s"));
    assert_eq!(first.seq, 0);
    assert_eq!(first.correlation_id.as_deref(), Some("req-7"));
    assert_eq!(first.approval_id, None);
    // The timestamp's shape, not its value: RFC 3339, millisecond
    // resolution, UTC — `2026-08-13T09:41:00.123Z` is 24 bytes.
    assert_eq!(first.ts.len(), 24);
    assert_eq!(&first.ts[10..11], "T");
    assert!(first.ts.ends_with('Z'));
    // Monotonic readings order with seq — that is what the field is for.
    let second = &events[1];
    assert!(first.monotonic_ns.unwrap() <= second.monotonic_ns.unwrap());
}

#[tokio::test]
async fn prefix_filter_exactly_namespace() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    // Both spellings of the same subscription — the wildcard form is
    // normalized, so the two must see identical streams.
    let mut dotted = bus
        .subscribe("s", EventFilter::Prefix("tool.".into()))
        .unwrap();
    let mut starred = bus
        .subscribe("s", EventFilter::Prefix("tool.*".into()))
        .unwrap();

    publisher.publish(token("noise")).unwrap();
    publisher.publish(tool_call("c1")).unwrap();
    publisher.publish(token("more noise")).unwrap();
    publisher.publish(tool_result("c1")).unwrap();
    bus.seal_session("s").unwrap();

    for subscription in [&mut dotted, &mut starred] {
        let events = drain(subscription, 2).await;
        assert_eq!(events[0].kind.event_type(), "tool.call_started");
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].kind.event_type(), "tool.result");
        assert_eq!(events[1].seq, 3);
        assert!(subscription.recv().await.is_none(), "an extra event leaked");
    }
}

#[tokio::test]
async fn exact_filter_exactly_type() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus
        .subscribe("s", EventFilter::Exact("stream.token".into()))
        .unwrap();

    publisher.publish(tool_call("c1")).unwrap();
    publisher.publish(token("kept")).unwrap();
    publisher.publish(tool_result("c1")).unwrap();
    bus.seal_session("s").unwrap();

    let events = drain(&mut subscription, 1).await;
    assert_eq!(events[0].kind.event_type(), "stream.token");
    assert_eq!(events[0].seq, 1);
    assert!(subscription.recv().await.is_none(), "an extra event leaked");
}

#[tokio::test]
async fn global_session_no_leak_either_way() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    let mut session_side = bus.subscribe("s", EventFilter::All).unwrap();
    let mut global_side = bus.subscribe_global(Vec::new());

    publisher.publish(token("scoped")).unwrap();
    bus.publish_global(notice("idle_prompt"));
    publisher.publish(token("scoped again")).unwrap();
    bus.seal_session("s").unwrap();

    let session_events = drain(&mut session_side, 2).await;
    assert!(
        session_events
            .iter()
            .all(|event| event.session_id.as_deref() == Some("s")),
        "a null-session event leaked onto a session subscription"
    );
    assert!(session_side.recv().await.is_none());

    let global_events = drain(&mut global_side, 1).await;
    assert_eq!(global_events[0].session_id, None);
    assert_eq!(global_events[0].kind.event_type(), "runtime.notice");
    // The global channel numbers its own stream: first global event is 0
    // even though the session had already stamped 0 for itself.
    assert_eq!(global_events[0].seq, 0);
}

#[tokio::test]
async fn sessions_are_isolated_with_independent_seq_domains() {
    let bus = EventBus::new(BusConfig::default());
    let publisher_a = bus.register_session("a".into()).unwrap();
    let publisher_b = bus.register_session("b".into()).unwrap();
    let mut sub_a = bus.subscribe("a", EventFilter::All).unwrap();
    let mut sub_b = bus.subscribe("b", EventFilter::All).unwrap();

    publisher_a.publish(token("a0")).unwrap();
    publisher_b.publish(token("b0")).unwrap();
    publisher_a.publish(token("a1")).unwrap();
    bus.seal_session("a").unwrap();
    bus.seal_session("b").unwrap();

    let events_a = drain(&mut sub_a, 2).await;
    assert!(
        events_a
            .iter()
            .all(|event| event.session_id.as_deref() == Some("a")),
        "another session's event leaked across"
    );
    assert_eq!((events_a[0].seq, events_a[1].seq), (0, 1));
    assert!(sub_a.recv().await.is_none());

    // Session b's stream numbers itself: its first event is 0 even though
    // session a had already stamped 0 and 1 — seq is a per-session domain,
    // never a bus-wide one.
    let events_b = drain(&mut sub_b, 1).await;
    assert_eq!(events_b[0].session_id.as_deref(), Some("b"));
    assert_eq!(events_b[0].seq, 0);
    assert!(sub_b.recv().await.is_none());
}

#[tokio::test]
async fn global_subscriptions_filter_by_namespace() {
    let bus = EventBus::new(BusConfig::default());
    let mut runtime_only = bus.subscribe_global(vec!["runtime".into()]);
    // A multi-namespace subscription admits an event matching any entry.
    let mut runtime_or_adapter = bus.subscribe_global(vec!["runtime".into(), "adapter".into()]);

    bus.publish_global(notice("one"));
    bus.publish_global(EventBody::new(EventKind::AdapterVersionWarning(
        agent_bridge_events::AdapterVersionWarning {
            adapter: None,
            detected_version: None,
            supported_range: None,
        },
    )));
    bus.publish_global(notice("two"));

    let events = drain(&mut runtime_only, 2).await;
    assert_eq!(events[0].kind.event_type(), "runtime.notice");
    assert_eq!(events[1].kind.event_type(), "runtime.notice");
    assert_eq!(
        events[1].seq, 2,
        "the adapter event was skipped, not renumbered"
    );

    let both = drain(&mut runtime_or_adapter, 3).await;
    assert_eq!(
        both.iter()
            .map(|event| event.kind.event_type().to_owned())
            .collect::<Vec<_>>(),
        [
            "runtime.notice",
            "adapter.version_warning",
            "runtime.notice"
        ],
    );
}

#[tokio::test]
async fn publish_nonblocking_under_stalled_subscriber() {
    const BOUND: usize = 64;
    const PUBLISHED: u64 = 10_000;

    let bus = EventBus::new(BusConfig {
        backpressure: BackpressureConfig {
            queue_bound: BOUND,
            ..BackpressureConfig::default()
        },
        ..BusConfig::default()
    });
    let publisher = bus.register_session("s".into()).unwrap();
    let mut stalled = bus.subscribe("s", EventFilter::All).unwrap();

    // The subscriber never reads while these run. Every publish completing
    // — synchronously, on this thread, far past the queue bound — is the
    // non-blocking assertion; a publish that waited for queue space would
    // hang the test right here.
    for i in 0..PUBLISHED {
        assert_eq!(publisher.publish(token("flood")).unwrap(), i);
    }
    bus.seal_session("s").unwrap();

    // Whatever the lag policy keeps for a stalled subscriber, it is a
    // subsequence of the stream in seq order — stated policy-free here;
    // the policy's own residue is pinned in the backpressure tests.
    let mut received = 0u64;
    let mut last: Option<u64> = None;
    while let Some(event) = stalled.recv().await {
        assert!(
            last.is_none_or(|previous| previous < event.seq),
            "delivery order diverged from seq order"
        );
        last = Some(event.seq);
        received += 1;
    }
    assert!(received > 0, "a stalled subscriber still keeps its queue");
    assert!(
        received < PUBLISHED,
        "a bound-{BOUND} queue cannot have kept all {PUBLISHED}"
    );
}

#[tokio::test]
async fn seal_session_drains_then_none() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    bus.seal_session("s").unwrap();
    assert_eq!(
        publisher.publish(token("late")),
        Err(BusError::Sealed("s".into()))
    );

    let events = drain(&mut subscription, 3).await;
    assert_eq!(events.last().unwrap().seq, 2);
    assert!(
        subscription.recv().await.is_none(),
        "sealed stream must end"
    );
}

/// Concurrent publishers with a bound small enough that a claiming
/// publisher hands the drainer role back mid-flight, repeatedly. The
/// handover moves delivery between threads while events keep arriving,
/// which is exactly where an ordering or loss bug would hide: every event
/// must still arrive exactly once, in `seq` order.
#[tokio::test]
async fn order_survives_the_drainer_role_moving_between_publishers() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 2_000;
    const TOTAL: u64 = THREADS * PER_THREAD;

    let bus = EventBus::new(BusConfig {
        backpressure: BackpressureConfig {
            // Small enough that a drain crosses it long before the
            // publishers are done, so the role changes hands often.
            queue_bound: 128,
            ..BackpressureConfig::default()
        },
        ..BusConfig::default()
    });
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let publisher = Arc::clone(&publisher);
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    publisher.publish(token("racing")).unwrap();
                }
            })
        })
        .collect();

    let events = drain(&mut subscription, TOTAL as usize).await;
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64, "torn ordering at position {i}");
    }
    assert_eq!(
        subscription.disconnect_reason(),
        None,
        "a subscriber draining as fast as it can must not be disconnected"
    );

    for handle in handles {
        handle.join().unwrap();
    }
}

#[tokio::test]
async fn publish_path_concurrency_stress() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 5_000;
    const TOTAL: u64 = THREADS * PER_THREAD;

    let bus = EventBus::new(BusConfig {
        // Roomy enough that nothing overflows even if the drain lags:
        // this test is about ordering through the choke point, not the
        // lag policy.
        backpressure: BackpressureConfig {
            queue_bound: TOTAL as usize,
            ..BackpressureConfig::default()
        },
        ..BusConfig::default()
    });
    let publisher = Arc::new(bus.register_session("s".into()).unwrap());
    let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let publisher = Arc::clone(&publisher);
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    publisher.publish(token("racing")).unwrap();
                }
            })
        })
        .collect();

    // Drain concurrently with the publishing threads: the queue order the
    // subscriber observes must be seq order exactly — stamped-but-pushed-
    // out-of-order interleavings are what the shared critical section
    // exists to forbid.
    let events = drain(&mut subscription, TOTAL as usize).await;
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64, "torn ordering at position {i}");
    }

    for handle in handles {
        handle.join().unwrap();
    }
}
