//! What a screen costs at its worst moment, measured rather than modelled.
//!
//! Every other memory figure in this component is counted: the grid is a
//! known number of cells of a known size, and counting keeps the number
//! independent of which allocator a test ran under. That works for a screen
//! sitting still and does not work for a screen in the middle of something,
//! because those costs are properties of the transition rather than of
//! either state — which is why counting kept looking complete and kept
//! being wrong. Four of them were found this way and none of them by
//! arithmetic: a reflow at thirty-one times the bound, a buffer replacement
//! at nearly two, a resize judged without the parked buffer beside it, and
//! one call to `feed` at seventeen.
//!
//! So these are weighed. A counting allocator records the high-water mark
//! across the operation, and the assertion is the promise the component makes
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
///
/// **What it does not do is isolate the allocator.** The harness itself
/// allocates while a test runs — collecting results, capturing output — and
/// none of that holds this lock. What the lock removes is the one source
/// that could matter: the other measurements in this file, which allocate
/// megabytes each. What is left is the harness's own traffic, kilobytes
/// against margins of 1.3 MiB at the tightest assertion here, so the
/// arithmetic that makes these deterministic is the margin rather than the
/// lock. Isolating properly would mean a child process per measurement,
/// which is worth doing the day a margin gets thin rather than now.
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
    // between them was 248 MiB — thirty-one times the eight-mebibyte bound
    // of the day, fifteen times the one that replaced it. A runtime
    // holding a few such sessions would have been killed by a window being
    // dragged narrower.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(2_000, 40);

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
fn one_enormous_feed_does_not_pass_through_the_bound() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // `feed` is public and takes a slice of any length, and both the decoder
    // and the emulator behind it allocate in proportion to what they are
    // handed at once. Measured before this was taken a piece at a time: an
    // eight-mebibyte call peaked at 142 MiB, and the same call in
    // undecodable bytes at 166 MiB, because each byte that cannot be read
    // becomes three of replacement character.
    //
    // The screen here is the default eighty by twenty-four, which settles at
    // 63 KiB. So the peak had nothing to do with the size of the screen, and
    // a bound expressed per session was being passed by a session whose
    // screen was two orders of magnitude inside it.
    for (what, input) in [
        ("plain text", vec![b'x'; 8 * 1024 * 1024]),
        ("undecodable bytes", vec![0xFF_u8; 8 * 1024 * 1024]),
    ] {
        let floor = LIVE.load(Ordering::Relaxed);
        let mut screen = ScreenState::new(80, 24, true);

        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        screen.feed(&input);
        let peak = PEAK.load(Ordering::Relaxed) - floor;

        assert!(
            peak <= LARGEST_SCREEN_BYTES,
            "one feed of eight mebibytes of {what} held {peak} B at its peak, past the \
             {LARGEST_SCREEN_BYTES} B this session was admitted under"
        );
    }
}

/// A screen where every cell carries a colour of its own.
///
/// The worst input a render can be given, and not an exotic one: an image
/// viewer, a gradient, a dashboard drawing a heat map. Every cell written, so
/// no row trims; every style distinct, so the table is as long as the grid
/// and the index that keeps it distinct is larger than both.
fn every_cell_a_different_colour(cols: u16, rows: u16) -> ScreenState {
    let mut screen = ScreenState::new(cols, rows, true);
    assert!(screen.is_kept(), "{cols}×{rows} is admitted");
    let mut colour = 0_u32;
    for row in 0..rows {
        let mut paint = format!("\u{1b}[{};1H", row + 1);
        for _ in 0..cols {
            let (r, g, b) = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF);
            paint.push_str(&format!("\u{1b}[38;2;{r};{g};{b}mX"));
            colour += 1;
        }
        screen.feed(paint.as_bytes());
    }
    screen.evaluate();
    screen
}

#[test]
fn rendering_the_largest_admitted_screen_stays_inside_the_bound() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The moment a reconnect pays for, weighed at the size where it costs
    // most. Three things exist at once and only the first was ever counted:
    // the grid, the snapshot being built from it, and the index that keeps
    // the style table distinct — which is the largest of the three, because
    // the published contract says each style is listed once and exact
    // deduplication needs a structure proportional to the number of them.
    //
    // Measured at 600×200 before this was part of admission: 15.5 MiB for a
    // screen holding 3.9, against a bound of 8. The peak-memory tests all
    // resized and none rendered, so the largest thing a session builds was
    // the one thing nothing weighed.
    for (cols, rows) in [(600, 200), (500, 150), (1_500, 40)] {
        let floor = LIVE.load(Ordering::Relaxed);
        let mut screen = every_cell_a_different_colour(cols, rows);

        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let snapshot = screen.render().expect("a kept screen renders");
        let peak = PEAK.load(Ordering::Relaxed) - floor;

        assert_eq!(
            snapshot.styles.len(),
            usize::from(cols) * usize::from(rows) + 1,
            "every cell really did get a style of its own, plus the default"
        );
        assert!(
            peak <= LARGEST_SCREEN_BYTES,
            "rendering a {cols}×{rows} screen painted a colour per cell held {peak} B at its \
             peak, past the {LARGEST_SCREEN_BYTES} B this session was admitted under"
        );
    }
}

#[test]
fn a_screen_cannot_resize_into_a_shape_it_could_not_have_been_created_in() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // A bound that holds at the front door and not at the side one is not a
    // bound. Creating a session judges the worst moment it can reach, which
    // includes building a snapshot; resizing one judged only what the
    // emulator does with its buffers, so a small screen could grow into a
    // shape `new` refuses and then render past the limit. Measured: 80×24
    // grown to 1 000×200 was admitted here and rendered at 16.1 MiB.
    //
    // Stated as the invariant rather than the arithmetic, because the
    // arithmetic is what drifted: whatever a session may be created as, it
    // may resize into, and nothing else.
    for (cols, rows) in [(1_000, 200), (800, 200), (700, 200), (600, 200), (80, 24)] {
        let creatable = ScreenState::new(cols, rows, true).is_kept();

        let mut grown = ScreenState::new(80, 24, true);
        grown.resize(cols, rows);

        assert_eq!(
            grown.is_kept(),
            creatable,
            "{cols}×{rows} can be created: {creatable}, but resized into: {}",
            grown.is_kept()
        );
    }
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
    let mut screen = painted(2_000, 40);
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
    let mut screen = painted(2_000, 40);
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
    let mut screen = painted(15, 5_000);

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.resize(400, 150);
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
    // Widening a hundredfold, and affordable because the row count is
    // small: forty rows at two thousand columns is 1.2 MiB held at once.
    // The guard is about what a reshape costs, not how large a change it is.
    let mut screen = ScreenState::new(20, 40, true);
    screen.feed(b"\x1b[1;1Hkeep me");
    screen.evaluate();

    screen.resize(2_000, 40);

    let snapshot = screen.render().expect("a kept screen renders");
    assert_eq!(snapshot.cols, 2_000, "the new size took");
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
    // The case the extreme ones hid: a narrowing the guard allows, close
    // enough to the edge that an under-count would show. 600×200 down to
    // 50×200 measures 13.9 MiB of the 16 MiB bound — and one more step, to
    // 40 columns, is refused, so this is the last shape on the allowed side.
    //
    // It was 500×200 when the bound was 8 MiB and this screen settled at 7.7
    // of it, where the point was that a screen near the ceiling has no room
    // for a reflow at all. Raising the bound left that pair sitting at a
    // third of it, which is a test that measures nothing: the shape moved so
    // the question would keep being asked.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(600, 200);

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.resize(50, 200);
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
    let mut screen = painted(600, 200);
    screen.feed(b"\x1b[1;1Hkeep me");
    screen.evaluate();

    screen.resize(600, 201);

    let snapshot = screen.render().expect("a kept screen renders");
    assert_eq!(snapshot.rows, 201, "the new size took");
    let first: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
    assert!(
        first.starts_with("keep me"),
        "a resize that reallocates nothing keeps the screen: {first:?}"
    );
}

#[test]
fn entering_the_alternate_screen_stays_inside_the_bound() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // No resize at all — just what every full-screen interface sends when it
    // starts, on a screen sitting near the ceiling it was admitted under.
    //
    // The emulator swaps its two buffers and then builds a fresh one over
    // the old alternate, so three exist for the length of that call, and a
    // reset builds both replacements before releasing either, so four do.
    // A screen admitted on what it holds at rest would walk through the
    // bound on the first `ESC[?1049h` of the session, with no resize to
    // blame and nothing to notice it. Admission covers the worst of the two
    // instead, which is what these two sequences check.
    let floor = LIVE.load(Ordering::Relaxed);
    let mut screen = painted(600, 200);
    assert!(screen.is_kept(), "the shape is admitted");

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.feed(b"\x1b[?1049h");
    let entering = PEAK.load(Ordering::Relaxed) - floor;
    assert!(
        entering <= LARGEST_SCREEN_BYTES,
        "entering the alternate screen held {entering} B, past the {LARGEST_SCREEN_BYTES} B \
         this screen was admitted under"
    );

    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    screen.feed(b"\x1bc");
    let resetting = PEAK.load(Ordering::Relaxed) - floor;
    assert!(
        resetting <= LARGEST_SCREEN_BYTES,
        "a reset held {resetting} B, past the {LARGEST_SCREEN_BYTES} B this screen was \
         admitted under"
    );
}

#[test]
fn a_screen_whose_resting_size_fits_but_whose_reset_does_not_is_refused() {
    let _measuring = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    // The shape that made the point. 1 200×200 holds 7.7 MiB at rest, which
    // is inside the bound and was how it used to be judged — and it reaches
    // 11.0 MiB entering the alternate screen and 14.7 MiB on a reset, both
    // measured, neither involving a resize. It is refused now.
    //
    // Asserted by refusal rather than by measurement, because there is no
    // longer any way to build one and watch it: that is what being refused
    // means. The measurement lives in the commit that found it, and the two
    // shapes below keep the refusal from being the trivial kind that would
    // also refuse everything.
    assert!(
        !ScreenState::new(1_200, 200, true).is_kept(),
        "a screen that would need three buffers past the bound was admitted on the two it \
         holds while nothing is happening to it"
    );
    assert!(
        ScreenState::new(600, 200, true).is_kept(),
        "and a shape that survives its own reset is still admitted"
    );
    assert!(
        ScreenState::new(80, 24, true).is_kept(),
        "as is the size a session usually gets"
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
