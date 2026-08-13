//! What one publish costs, at the fanout widths that matter: no
//! subscribers (the envelope-stamping floor), one (the common case), and
//! eight (a session watched by an IDE, a script, and loggers at once).
//!
//! Two numbers per width, latency percentiles and allocations, because the
//! publish path sits on the many-sessions-many-events-per-second envelope
//! and both dimensions bound it: latency says what the critical section
//! costs under one caller, allocations say what the allocator will be asked
//! for a million times an hour. Recorded, not gated — the throughput budget
//! this path must fit inside belongs to the SLO harness, and a threshold
//! here before that harness states it would be a number defending itself.
//!
//! Timed publishes run in chunks with untimed drains between them, so every
//! measured publish sees a steady-state queue. Letting the queues grow for
//! the whole run instead would fold the channel's buffer-growth
//! allocations into the figures — a regime a drained consumer never pays,
//! and a distortion of exactly the numbers a future gate would be
//! calibrated against.

#![allow(
    clippy::disallowed_macros,
    reason = "a benchmark's report is its output, and it is run by hand or by the bench lane \
              rather than by the runtime — nothing is reading a protocol on this stdout"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use agent_bridge_core::{BusConfig, EventBus, EventFilter, Subscription};
use agent_bridge_events::{EventBody, EventKind, StreamToken};

/// The system allocator with a call counter in front — the only way to see
/// allocations from inside the process, and the reason this binary carries
/// `unsafe` while the library forbids it: `GlobalAlloc` is an unsafe trait,
/// and this impl does nothing but count and forward.
struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded verbatim to the system allocator; the caller's
        // obligations are exactly `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above — a pure pass-through.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above — a pure pass-through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above — a pure pass-through.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Timed publishes per chunk; the queue bound is twice this, so a timed
/// chunk never overflows and never touches the (interim, differently
/// costed) overflow path.
const CHUNK: usize = 1_000;

/// Timed chunks per fanout width; one extra untimed chunk warms up.
const CHUNKS: usize = 100;

fn main() {
    // `cargo bench` forwards its own harness flags; ignore them.
    for subscriber_count in [0usize, 1, 8] {
        let report = measure(subscriber_count);
        println!(
            "publish_path: {subscriber_count} subscribers: {} publishes, \
             p50 {} ns, p99 {} ns, max {} ns, {:.2} allocations/publish",
            CHUNKS * CHUNK,
            report.p50_ns,
            report.p99_ns,
            report.max_ns,
            report.allocations_per_publish,
        );
    }
}

struct Report {
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    allocations_per_publish: f64,
}

fn measure(subscriber_count: usize) -> Report {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime for the untimed drains");
    let bus = EventBus::new(BusConfig {
        subscriber_queue_bound: CHUNK * 2,
    });
    let publisher = bus.register_session("bench".into()).unwrap();
    let mut subscriptions: Vec<_> = (0..subscriber_count)
        .map(|_| bus.subscribe("bench", EventFilter::All).unwrap())
        .collect();

    for _ in 0..CHUNK {
        black_box(publisher.publish(body()).unwrap());
    }
    drain_chunk(&runtime, &mut subscriptions);

    let mut samples = Vec::with_capacity(CHUNKS * CHUNK);
    let mut allocations: u64 = 0;
    for _ in 0..CHUNKS {
        for _ in 0..CHUNK {
            // The body is built outside the measured window: constructing
            // the producer's event is the producer's cost, and mixing it
            // in would overstate the bus's.
            let event = body();
            let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            black_box(publisher.publish(event).unwrap());
            let elapsed = start.elapsed();
            allocations += ALLOCATIONS.load(Ordering::Relaxed) - allocations_before;
            samples.push(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
        }
        // Untimed: return every queue to empty so the next timed chunk
        // measures steady-state delivery, not the buffer-growth regime.
        drain_chunk(&runtime, &mut subscriptions);
    }

    samples.sort_unstable();
    Report {
        p50_ns: percentile(&samples, 50),
        p99_ns: percentile(&samples, 99),
        max_ns: *samples.last().unwrap(),
        allocations_per_publish: allocations as f64 / (CHUNKS * CHUNK) as f64,
    }
}

fn drain_chunk(runtime: &tokio::runtime::Runtime, subscriptions: &mut [Subscription]) {
    for subscription in subscriptions.iter_mut() {
        runtime.block_on(async {
            for _ in 0..CHUNK {
                subscription
                    .recv()
                    .await
                    .expect("every published event of the chunk is queued");
            }
        });
    }
}

fn body() -> EventBody {
    EventBody::new(EventKind::StreamToken(StreamToken {
        source: None,
        content: "a representative token of ordinary length".into(),
    }))
}

/// Element `ceil(percent/100 × n)` of an ascending slice, 1-indexed — the
/// same nearest-rank definition the perf probe's stats use, so this
/// bench's percentiles and the SLO lane's mean the same thing.
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}
