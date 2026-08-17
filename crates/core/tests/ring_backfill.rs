//! The ring and backfill contract, through the public surface only: the
//! three replay shapes and their wire serialization, replay-then-live
//! contiguity at the seam (deterministic sweep and concurrent race), FIFO
//! eviction under the count bound, the oversized-event tension, the budget
//! instrumentation, and the refusals. The age bound's tests live with the
//! ring itself (`src/bus/ring.rs`), where instants can be fabricated —
//! there is no way to age a ring through this surface without sleeping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use agent_bridge_core::{
    BusConfig, BusError, EventBus, EventFilter, ReplayPlan, RingConfig, Subscription,
};
use agent_bridge_events::{
    CursorPosition, Event, EventBody, EventKind, LifecycleTurnCompleted, LifecycleTurnStarted,
    ScreenSnapshot, SessionReconnected, SessionReconnecting, StreamToken, ToolCallStarted,
    ToolResult,
};

fn token(content: &str) -> EventBody {
    EventBody::new(EventKind::StreamToken(StreamToken {
        source: None,
        content: content.into(),
    }))
}

fn small_ring_bus(max_events: usize) -> EventBus {
    EventBus::new(BusConfig {
        ring: RingConfig {
            max_events,
            max_age: Duration::from_secs(300),
        },
        ..BusConfig::default()
    })
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

/// A seedable pseudo-random step (Knuth's MMIX constants) — deterministic
/// drop points without a dependency, reproducible from the seed alone.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

#[tokio::test]
async fn within_ring_replay_exact_then_live_seamless() {
    const RING: u64 = 64;
    const ROUNDS: u32 = 60;

    let bus = small_ring_bus(RING as usize);
    let publisher = bus.register_session("s".into()).unwrap();
    let mut seed = 0x1_7b;
    let mut head: u64 = 0;

    // Every round: advance the stream, re-attach at a pseudo-random drop
    // point, and require the exact contract — `head − from_seq` events
    // replayed in order when the point is still held, the gap shape naming
    // the true earliest when it is not, and in both cases live events
    // continuing seamlessly from the attach head. Expected values are
    // computed independently from the round's arithmetic, so an
    // off-by-one at the seam has nowhere to hide.
    for _ in 0..ROUNDS {
        for _ in 0..lcg(&mut seed) % 40 {
            publisher.publish(token("x")).unwrap();
            head += 1;
        }
        let from_seq = lcg(&mut seed) % (head + 1);
        let earliest = head.saturating_sub(RING);

        let (mut subscription, plan) = bus
            .subscribe_from("s", Some(from_seq), EventFilter::All)
            .unwrap();
        let mut expected = if from_seq >= earliest {
            assert_eq!(
                plan,
                ReplayPlan::WithinRing {
                    // The first seq actually delivered: the request itself
                    // on this unfiltered path, or nothing at head.
                    replayed_from: (from_seq < head).then_some(from_seq),
                    events_replayed: head - from_seq,
                },
                "drop point {from_seq} of head {head}"
            );
            let replayed = drain(&mut subscription, (head - from_seq) as usize).await;
            for (offset, event) in replayed.iter().enumerate() {
                assert_eq!(event.seq, from_seq + offset as u64);
            }
            from_seq + replayed.len() as u64
        } else {
            assert_eq!(
                plan,
                ReplayPlan::Gap {
                    earliest_seq: earliest
                }
            );
            head
        };

        let live = lcg(&mut seed) % 20 + 1;
        for _ in 0..live {
            publisher.publish(token("live")).unwrap();
            head += 1;
        }
        for event in drain(&mut subscription, live as usize).await {
            assert_eq!(event.seq, expected, "gap or duplicate at the seam");
            expected += 1;
        }
    }
}

#[tokio::test]
async fn seam_holds_under_concurrent_publish() {
    const ATTACHES: u32 = 40;

    let bus = small_ring_bus(64);
    let publisher = bus.register_session("s".into()).unwrap();
    let start = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicBool::new(false));

    // The publisher stops (and only then seals) once every attach below
    // has completed, so all of them race live publishing by construction —
    // scheduler timing cannot make this test vacuous. Pacing keeps the
    // race honest without flakes: fast enough that every drain in the
    // attach loop is answered promptly, slow enough that a subscriber
    // draining its replay cannot be overflowed by the flood.
    let publisher_thread = {
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        let bus = bus.clone();
        thread::spawn(move || {
            start.wait();
            while !done.load(Ordering::Acquire) {
                publisher.publish(token("racing")).unwrap();
                thread::sleep(Duration::from_micros(20));
            }
            bus.seal_session("s").unwrap();
        })
    };
    start.wait();

    // The exact head is unknowable mid-flood, so the assertions are the
    // invariants that hold at every interleaving: a within-ring plan
    // starts delivery exactly at its drop point, a gap plan starts at or
    // past its stated earliest, and from the first delivered event on the
    // stream is consecutive — which is precisely what the seam critical
    // section promises.
    let mut seed = 0xace;
    for _ in 0..ATTACHES {
        let head = bus.ring_stats("s").unwrap().head_seq;
        let from_seq = lcg(&mut seed) % (head + 1);
        let (mut subscription, plan) = bus
            .subscribe_from("s", Some(from_seq), EventFilter::All)
            .expect("the session seals only after every attach has completed");
        let mut expected = match plan {
            ReplayPlan::WithinRing {
                replayed_from,
                events_replayed,
            } => {
                match replayed_from {
                    Some(replayed_from) => assert_eq!(replayed_from, from_seq),
                    None => assert_eq!(
                        events_replayed, 0,
                        "null replayed_from means nothing delivered"
                    ),
                }
                let replayed = drain(&mut subscription, events_replayed as usize).await;
                for (offset, event) in replayed.iter().enumerate() {
                    assert_eq!(event.seq, from_seq + offset as u64);
                }
                from_seq + events_replayed
            }
            ReplayPlan::Gap { earliest_seq } => {
                assert!(from_seq < earliest_seq);
                let event = subscription
                    .recv()
                    .await
                    .expect("the publisher stays live until every attach has completed");
                assert!(
                    event.seq >= earliest_seq,
                    "gap attach delivered evicted seq"
                );
                event.seq + 1
            }
            ReplayPlan::LiveFromHead => unreachable!("from_seq was supplied"),
        };
        for _ in 0..5 {
            let event = subscription
                .recv()
                .await
                .expect("the publisher stays live until every attach has completed");
            assert_eq!(event.seq, expected, "gap or duplicate at the seam");
            expected += 1;
        }
    }
    done.store(true, Ordering::Release);
    publisher_thread.join().unwrap();
}

#[tokio::test]
async fn out_of_ring_gap_with_earliest_seq() {
    let bus = small_ring_bus(100);
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..250 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    let (mut subscription, plan) = bus.subscribe_from("s", Some(10), EventFilter::All).unwrap();
    assert_eq!(plan, ReplayPlan::Gap { earliest_seq: 150 });

    // Attached at head regardless of the gap: nothing replayed, and the
    // next publish is the first delivery.
    publisher.publish(token("post-attach")).unwrap();
    assert_eq!(drain(&mut subscription, 1).await[0].seq, 250);
}

#[tokio::test]
async fn no_from_seq_live_from_head() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..5 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    let (mut subscription, plan) = bus.subscribe_from("s", None, EventFilter::All).unwrap();
    assert_eq!(plan, ReplayPlan::LiveFromHead);

    let info = plan.to_replay_info(None);
    assert_eq!(info.replayed_from, None);
    assert_eq!(info.events_replayed, 0);
    assert!(!info.gap);

    publisher.publish(token("live")).unwrap();
    assert_eq!(drain(&mut subscription, 1).await[0].seq, 5);
}

#[test]
fn count_bound_evicts_fifo() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    // One past the default 10,000-event bound: exactly seq 0 ages out, by
    // count alone — the wall-clock bound is nowhere near.
    for i in 0..10_001 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    let stats = bus.ring_stats("s").unwrap();
    assert_eq!(stats.events, 10_000);
    assert_eq!(stats.earliest_seq, Some(1));
    assert_eq!(stats.head_seq, 10_001);

    let (_, plan) = bus.subscribe_from("s", Some(0), EventFilter::All).unwrap();
    assert_eq!(plan, ReplayPlan::Gap { earliest_seq: 1 });
    let (_, plan) = bus.subscribe_from("s", Some(1), EventFilter::All).unwrap();
    assert_eq!(
        plan,
        ReplayPlan::WithinRing {
            replayed_from: Some(1),
            events_replayed: 10_000,
        }
    );
}

#[tokio::test]
async fn disabled_ring_reports_gap_from_head() {
    let bus = small_ring_bus(0);
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    // Retention disabled leaves no earliest entry to name, so the gap
    // reports head — the first position that cannot have been lost — and
    // the attach still lands live at head like every other outcome.
    let (mut subscription, plan) = bus.subscribe_from("s", Some(0), EventFilter::All).unwrap();
    assert_eq!(plan, ReplayPlan::Gap { earliest_seq: 3 });

    publisher.publish(token("live")).unwrap();
    assert_eq!(drain(&mut subscription, 1).await[0].seq, 3);
}

#[tokio::test]
async fn oversized_event_evicts_ring_then_gap() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();

    // The frame-cap-vs-ring-budget tension, encoded: a ~10 MiB
    // `stream.token` is admitted whole — the ring bounds count and age,
    // never bytes — so the budget instrumentation must show it resident.
    // Note what does *not* happen: under the dual bound the insert itself
    // evicts nothing. The whale leaves the way everything leaves, by count
    // or age churn, and what the contract owes the subscriber is only that
    // the backfill afterwards reports the gap honestly.
    publisher
        .publish(token(&"x".repeat(10 * 1024 * 1024)))
        .unwrap();
    assert!(
        bus.ring_stats("s").unwrap().approx_bytes > 10 * 1024 * 1024,
        "the oversized resident event must be visible to the budget row"
    );

    for i in 0..10_000 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    let stats = bus.ring_stats("s").unwrap();
    assert_eq!(stats.earliest_seq, Some(1), "the whale was evicted FIFO");
    // What remains is 10k small events (a few MB of estimate); the bound
    // proves the whale's ~10 MiB left the accounting with it.
    assert!(
        stats.approx_bytes < 10 * 1024 * 1024,
        "eviction must give back the whale's accounted bytes; still counting {}",
        stats.approx_bytes
    );

    let (mut subscription, plan) = bus.subscribe_from("s", Some(0), EventFilter::All).unwrap();
    assert_eq!(plan, ReplayPlan::Gap { earliest_seq: 1 });
    let info = plan.to_replay_info(None);
    assert!(info.gap);
    assert_eq!(info.earliest_seq, Some(1));

    publisher.publish(token("live")).unwrap();
    assert_eq!(drain(&mut subscription, 1).await[0].seq, 10_001);
}

#[test]
fn ring_budget_10k_typical_events() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();

    // The fake-CLI shape of a session's stream: overwhelmingly short
    // tokens, with tool-call and turn-lifecycle events sprinkled through —
    // the workload the ~2.5 MiB ring budget (10k events × ~256 B average)
    // was priced against.
    for i in 0u64..10_000 {
        let body = match i % 20 {
            17 => EventBody::new(EventKind::ToolCallStarted(ToolCallStarted {
                call_id: format!("call-{i}"),
                tool: "bash".into(),
                command: Some("cargo build".into()),
            })),
            18 => EventBody::new(EventKind::ToolResult(ToolResult {
                call_id: format!("call-{i}"),
                content: "ok: finished in 1.2s".into(),
            })),
            19 if i % 40 == 19 => {
                EventBody::new(EventKind::LifecycleTurnStarted(LifecycleTurnStarted {}))
            }
            19 => EventBody::new(EventKind::LifecycleTurnCompleted(LifecycleTurnCompleted {})),
            _ => token(&"word ".repeat((i % 6) as usize + 1)),
        };
        publisher.publish(body).unwrap();
    }

    let stats = bus.ring_stats("s").unwrap();
    assert_eq!(stats.events, 10_000);
    // The envelope: the ~2.5 MiB budget with a generous tolerance — the
    // estimator's fixed per-event struct cost is real memory the 256 B
    // average never priced, and the soak harness is what turns this
    // budget into a hard number.
    let budget = 5 * 1024 * 1024 / 2;
    assert!(
        stats.approx_bytes <= budget * 3 / 2,
        "10k typical events estimate {} exceeds 1.5× the ~2.5 MiB budget row \
         (fixed struct cost dominates it: {} B/event × 10k — a grown Event \
         or EventKind variant moves this bound without touching the ring)",
        stats.approx_bytes,
        std::mem::size_of::<agent_bridge_events::Event>()
    );
    assert!(
        stats.approx_bytes >= 10_000 * 100,
        "estimate {} is implausibly small — the accounting is undercounting",
        stats.approx_bytes
    );
}

#[test]
fn replay_info_serializes_the_three_wire_shapes() {
    let within = ReplayPlan::WithinRing {
        replayed_from: Some(17),
        events_replayed: 3,
    }
    .to_replay_info(None);
    assert_eq!(
        serde_json::to_value(&within).unwrap(),
        serde_json::json!({ "replayed_from": 17, "events_replayed": 3, "gap": false })
    );

    // A within-ring plan that delivered nothing is the live shape on the
    // wire: replayed_from is null exactly when nothing was replayed.
    let empty_within = ReplayPlan::WithinRing {
        replayed_from: None,
        events_replayed: 0,
    }
    .to_replay_info(None);
    assert_eq!(
        serde_json::to_value(&empty_within).unwrap(),
        serde_json::json!({ "replayed_from": null, "events_replayed": 0, "gap": false })
    );

    let snapshot = ScreenSnapshot {
        cols: 4,
        rows: 1,
        cursor: CursorPosition { row: 0, col: 2 },
        styles: vec![agent_bridge_events::CellStyle::default()],
        cells: vec![vec![
            agent_bridge_events::ScreenCell::plain('h'),
            agent_bridge_events::ScreenCell::plain('i'),
        ]],
    };
    let gap = ReplayPlan::Gap { earliest_seq: 42 }.to_replay_info(Some(snapshot));
    assert_eq!(
        serde_json::to_value(&gap).unwrap(),
        serde_json::json!({
            "replayed_from": null,
            "events_replayed": 0,
            "gap": true,
            "earliest_seq": 42,
            "screen_snapshot": {
                "cols": 4,
                "rows": 1,
                "cursor": { "row": 0, "col": 2 },
                "styles": [{}],
                "cells": [[{ "ch": "h" }, { "ch": "i" }]],
            },
        })
    );

    // No snapshot supplied — effective tui_aware off — omits the key
    // entirely: the degraded contract's spelling is an absent key, never
    // a null.
    let degraded =
        serde_json::to_value(ReplayPlan::Gap { earliest_seq: 42 }.to_replay_info(None)).unwrap();
    assert!(degraded.get("screen_snapshot").is_none());
    assert_eq!(degraded["gap"], serde_json::json!(true));

    let live = serde_json::to_value(ReplayPlan::LiveFromHead.to_replay_info(None)).unwrap();
    assert_eq!(
        live,
        serde_json::json!({ "replayed_from": null, "events_replayed": 0, "gap": false })
    );
}

/// The reconnect control events, constructed the way the session layer
/// will construct them (their emitting call sites land with the session
/// class and the wire attach): events in shape and vocabulary,
/// subscription-scoped notifications in delivery — never published, so
/// they never enter the ring or take a `seq`.
#[test]
fn reconnect_control_events_serialize_to_wire_shape() {
    let reconnecting = EventKind::SessionReconnecting(SessionReconnecting {
        from_seq: Some(100),
        subscriber: "ide-1".into(),
    });
    assert_eq!(reconnecting.event_type(), "session.reconnecting");
    assert_eq!(
        serde_json::to_value(&reconnecting).unwrap(),
        serde_json::json!({
            "type": "session.reconnecting",
            "payload": { "from_seq": 100, "subscriber": "ide-1" },
        })
    );

    let plan = ReplayPlan::WithinRing {
        replayed_from: Some(100),
        events_replayed: 40,
    };
    let reconnected = EventKind::SessionReconnected(SessionReconnected {
        replay: plan.to_replay_info(None),
    });
    assert_eq!(reconnected.event_type(), "session.reconnected");
    assert_eq!(
        serde_json::to_value(&reconnected).unwrap(),
        serde_json::json!({
            "type": "session.reconnected",
            "payload": {
                "replay": { "replayed_from": 100, "events_replayed": 40, "gap": false },
            },
        })
    );
}

#[test]
fn gap_snapshot_populated_iff_supplied() {
    fn snapshot() -> ScreenSnapshot {
        ScreenSnapshot {
            cols: 1,
            rows: 1,
            cursor: CursorPosition::default(),
            styles: vec![agent_bridge_events::CellStyle::default()],
            cells: vec![Vec::new()],
        }
    }

    let plan = ReplayPlan::Gap { earliest_seq: 7 };
    assert!(
        plan.to_replay_info(Some(snapshot()))
            .screen_snapshot
            .is_some()
    );
    assert!(plan.to_replay_info(None).screen_snapshot.is_none());
    // The non-gap shapes lost nothing a snapshot could stand in for, so
    // one supplied anyway must be discarded — asserted with a real
    // snapshot in hand, or the claim would be vacuously true of None.
    assert!(
        ReplayPlan::LiveFromHead
            .to_replay_info(Some(snapshot()))
            .screen_snapshot
            .is_none()
    );
    assert!(
        ReplayPlan::WithinRing {
            replayed_from: Some(3),
            events_replayed: 4,
        }
        .to_replay_info(Some(snapshot()))
        .screen_snapshot
        .is_none()
    );
}

#[tokio::test]
async fn filtered_backfill_replays_only_matching() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..6 {
        publisher.publish(token(&i.to_string())).unwrap();
        publisher
            .publish(EventBody::new(EventKind::ToolCallStarted(
                ToolCallStarted {
                    call_id: format!("call-{i}"),
                    tool: "bash".into(),
                    command: None,
                },
            )))
            .unwrap();
    }

    // The replay slice passes the subscription's own filter, and the plan
    // reports what is delivered: `replayed_from` is the first admitted
    // event (seq 1 here — the request named 0, a token the filter drops),
    // `events_replayed` the post-filter count. The v1 wire attach is
    // unfiltered (this path serves bus-level subscribers), so the wire
    // arithmetic `head − from_seq` is unaffected.
    let (mut subscription, plan) = bus
        .subscribe_from("s", Some(0), EventFilter::Prefix("tool.".into()))
        .unwrap();
    assert_eq!(
        plan,
        ReplayPlan::WithinRing {
            replayed_from: Some(1),
            events_replayed: 6,
        }
    );
    for (i, event) in drain(&mut subscription, 6).await.iter().enumerate() {
        assert_eq!(event.kind.event_type(), "tool.call_started");
        assert_eq!(event.seq, i as u64 * 2 + 1);
    }

    // A filter that admits nothing held delivers nothing: null
    // replayed_from, and the wire shape is live-from-head.
    let (_, plan) = bus
        .subscribe_from("s", Some(0), EventFilter::Exact("stream.stderr".into()))
        .unwrap();
    assert_eq!(
        plan,
        ReplayPlan::WithinRing {
            replayed_from: None,
            events_replayed: 0,
        }
    );
}

#[test]
fn from_seq_beyond_head_is_refused() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..3 {
        publisher.publish(token(&i.to_string())).unwrap();
    }

    assert_eq!(
        bus.subscribe_from("s", Some(7), EventFilter::All)
            .map(|(_, plan)| plan),
        Err(BusError::FromSeqBeyondHead {
            session_id: "s".into(),
            from_seq: 7,
            head: 3,
        })
    );
    // The boundary of the refusal: head itself is a valid resume point —
    // the subscriber that saw everything and missed nothing. Nothing is
    // delivered, so the wire's replayed_from is null (the live shape).
    let (_, plan) = bus.subscribe_from("s", Some(3), EventFilter::All).unwrap();
    assert_eq!(
        plan,
        ReplayPlan::WithinRing {
            replayed_from: None,
            events_replayed: 0,
        }
    );
    assert_eq!(
        serde_json::to_value(plan.to_replay_info(None)).unwrap(),
        serde_json::json!({ "replayed_from": null, "events_replayed": 0, "gap": false })
    );
}

#[test]
fn subscribe_from_refuses_unknown_and_sealed() {
    let bus = EventBus::new(BusConfig::default());
    assert!(matches!(
        bus.subscribe_from("ghost", Some(0), EventFilter::All),
        Err(BusError::UnknownSession(id)) if id == "ghost"
    ));

    let publisher = bus.register_session("s".into()).unwrap();
    publisher.publish(token("only")).unwrap();
    bus.seal_session("s").unwrap();
    assert!(matches!(
        bus.subscribe_from("s", Some(0), EventFilter::All),
        Err(BusError::Sealed(_))
    ));
}

#[test]
fn seal_clears_the_ring() {
    let bus = EventBus::new(BusConfig::default());
    let publisher = bus.register_session("s".into()).unwrap();
    for i in 0..100 {
        publisher.publish(token(&i.to_string())).unwrap();
    }
    assert_eq!(bus.ring_stats("s").unwrap().events, 100);

    bus.seal_session("s").unwrap();
    // A sealed session can never admit a subscriber to replay to, so the
    // ring's memory is released with the seal; the seq domain it stamped
    // remains on record.
    let stats = bus.ring_stats("s").unwrap();
    assert_eq!(stats.events, 0);
    assert_eq!(stats.approx_bytes, 0);
    assert_eq!(stats.earliest_seq, None);
    assert_eq!(stats.head_seq, 100);
}
