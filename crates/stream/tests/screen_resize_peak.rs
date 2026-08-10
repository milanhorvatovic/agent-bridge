//! What a resize costs while it is happening, measured rather than modelled.
//!
//! Every other memory figure in this component is counted: the grid is a
//! known number of cells of a known size, and counting keeps the number
//! independent of which allocator a test ran under. That works for a screen
//! sitting still and does not work for a screen changing shape, because the
//! cost of a reflow is not a property of either shape — it is a property of
//! how the emulator gets from one to the other, and it was thirty-one times
//! the bound for a screen both of whose shapes were comfortably inside it.
//!
//! So this one is weighed. A counting allocator records the high-water mark
//! across the resize, and the assertion is the promise the component makes
//! to whatever budgets sessions from it: a session's screen does not hold
//! more than [`LARGEST_SCREEN_BYTES`], transients included. A bound that
//! only holds once the dust settles is advisory, and nothing that has run
//! out of memory is comforted by what the figure became afterwards.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_bridge_stream::{LARGEST_SCREEN_BYTES, ScreenState};

/// Bytes currently handed out, and the most there have ever been at once.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, keeping a running total on the way past.
///
/// `Relaxed` throughout: the counters are read only by the test holding
/// [`ONE_AT_A_TIME`], so they need to agree with each other and with nothing
/// else.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            // Both live at once for as long as the copy takes.
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Held for the length of every test in this file.
///
/// The counters above are process-wide and the harness runs tests in
/// parallel, so a second test allocating during the measured window would be
/// charged to the screen being weighed. That failure would be intermittent
/// and would read as a memory bug, which is the worst way to learn that a
/// test was measuring the wrong thing.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fills every row of a `cols`×`rows` screen with text.
fn painted(cols: u16, rows: u16) -> ScreenState {
    let mut screen = ScreenState::new(cols, rows, true);
    assert!(screen.is_kept(), "{cols}×{rows} is inside the bound");
    for row in 0..rows {
        let mut paint = format!("\u{1b}[{};1H", row + 1);
        paint.push_str(&"abcdefghij".repeat(usize::from(cols) / 10));
        screen.feed(paint.as_bytes());
    }
    screen.evaluate();
    screen
}

#[test]
fn narrowing_a_wide_screen_stays_inside_the_bound_while_it_happens() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The shape that found it: both ends are affordable — 4 000×40 settles
    // around five megabytes and 20×40 is a rounding error — and the journey
    // between them was 248 MiB, thirty-one times the bound. A runtime
    // holding a few such sessions would have been killed by a window being
    // dragged narrower.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(4_000, 40);

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.resize(20, 40);
    let peak = PEAK.load(Ordering::Relaxed) - floor;

    assert!(
        peak <= LARGEST_SCREEN_BYTES,
        "narrowing held {peak} B at its peak, past the {LARGEST_SCREEN_BYTES} B this screen \
         was admitted under"
    );
}

#[test]
fn a_reshape_too_expensive_to_reflow_ends_the_screen() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The cost of the bound, asserted so that it is a decision rather than a
    // surprise. The session keeps no screen afterwards and says so, which is
    // a state callers already have to handle — a terminal larger than can be
    // reconstructed reaches it too.
    let mut screen = painted(4_000, 40);
    assert!(screen.is_kept(), "there is a screen to lose");

    screen.resize(20, 40);

    assert!(!screen.is_kept(), "the session stopped keeping a screen");
    assert_eq!(screen.render(), None, "and says so rather than rendering");
    assert_eq!(screen.footprint(), 0, "and is holding nothing");
}

#[test]
fn a_screen_is_not_continued_from_a_state_the_terminal_never_established() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // Why the screen goes rather than being replaced. An emulator cannot be
    // handed new buffers while keeping what it accumulated, so a replacement
    // starts with a default pen, default modes, and its primary buffer live.
    //
    // The second of those is the one that matters here. A CLI that has
    // switched to the alternate screen — which is where the recorded
    // interfaces spend nearly all of their time — would go on drawing there
    // while the reconstruction drew on the primary, and the two would
    // disagree about every subsequent byte, silently, for the rest of the
    // session. A screen that is confidently wrong is worse than no screen,
    // because what it reports is indistinguishable from what a correct one
    // reports.
    let mut screen = painted(4_000, 40);
    screen.feed(b"\x1b[?1049h\x1b[31m\x1b[1;1Hon the alternate screen");
    screen.evaluate();

    screen.resize(20, 40);

    assert!(
        !screen.is_kept(),
        "a session whose emulator state cannot be carried across keeps no screen"
    );
}

#[test]
fn widening_a_tall_screen_stays_inside_the_bound_while_it_happens() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The direction that looks harmless and is not. Nothing is split here —
    // every row simply becomes wider — but the reflow expands all twelve
    // thousand of them before the buffer keeps a hundred and fifty, so the
    // peak is the old row count at the new width. 95.6 MiB, for a reshape
    // whose two ends are 5.8 MiB and 1.2 MiB.
    //
    // This shape is the one the settled projection already calls out as the
    // expensive cross-shape case, which is what makes it worth having: the
    // projection was right about the shapes and had nothing to say about the
    // journey.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(15, 12_000);

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.resize(500, 150);
    let peak = PEAK.load(Ordering::Relaxed) - floor;

    assert!(
        peak <= LARGEST_SCREEN_BYTES,
        "widening held {peak} B at its peak, past the {LARGEST_SCREEN_BYTES} B this screen \
         was admitted under"
    );
}

#[test]
fn an_ordinary_widening_still_reflows() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // Widening far more than the narrowing test narrows, and affordable
    // because the row count is small: 40 rows at 4 000 columns is 2.5 MiB
    // held at once. The guard is about what a reshape costs, not how big a
    // change it is.
    let mut screen = ScreenState::new(20, 40, true);
    screen.feed(b"\x1b[1;1Hkeep me");
    screen.evaluate();

    screen.resize(4_000, 40);

    let snapshot = screen.render().expect("a kept screen renders");
    assert_eq!(snapshot.cols, 4_000, "the new size took");
    let first: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
    assert!(
        first.starts_with("keep me"),
        "an affordable widening keeps what was on the screen: {first:?}"
    );
}

#[test]
fn a_screen_near_the_limit_stays_inside_it_through_a_modest_narrowing() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The case the extreme ones hid. Both shapes here are ordinary and the
    // reflow between them is affordable on its own — 1 200×200 narrowing to
    // 1 000×200 is a fifth off the width, not a factor of two hundred — but
    // the screen already sits at 7.7 MiB of the 8 MiB it was admitted under,
    // so there is no room for the reflow to happen in. Measured at 10.4 MiB
    // when the guard weighed only the reflow's own share.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(1_200, 200);

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.resize(1_000, 200);
    let peak = PEAK.load(Ordering::Relaxed) - floor;

    assert!(
        peak <= LARGEST_SCREEN_BYTES,
        "a near-limit narrowing held {peak} B at its peak, past the {LARGEST_SCREEN_BYTES} B \
         this screen was admitted under"
    );
}

#[test]
fn changing_only_the_row_count_still_reflows() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The other side of guarding on the total, and the reason the model
    // separates a reshape that reallocates from one that does not. A screen
    // this close to the bound has room for nothing — so if a resize at the
    // same width were charged for a buffer it never allocates, every near-
    // limit session would lose its screen to a window one row taller.
    // Measured: 33 KiB, because the rows are moved rather than rebuilt.
    let mut screen = painted(1_200, 200);
    screen.feed(b"\x1b[1;1Hkeep me");
    screen.evaluate();

    screen.resize(1_200, 201);

    let snapshot = screen.render().expect("a kept screen renders");
    assert_eq!(snapshot.rows, 201, "the new size took");
    let first: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
    assert!(
        first.starts_with("keep me"),
        "a resize that reallocates nothing keeps the screen: {first:?}"
    );
}

#[test]
fn an_ordinary_narrowing_still_reflows() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The other half of the trade. Rebuilding is for the narrowing that
    // cannot be afforded, and a test that only proved the expensive case is
    // handled would be satisfied by a component that threw the screen away
    // every time anyone resized anything.
    let mut screen = painted(120, 24);
    screen.feed(b"\x1b[1;1Hkeep me");
    screen.evaluate();

    screen.resize(80, 24);

    let snapshot = screen.render().expect("a kept screen renders");
    assert_eq!(snapshot.cols, 80, "the new size took");
    let first: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
    assert!(
        first.starts_with("keep me"),
        "an affordable narrowing keeps what was on the screen: {first:?}"
    );
}
