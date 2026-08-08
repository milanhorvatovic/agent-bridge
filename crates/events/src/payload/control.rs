//! `session.*` — the control events a subscriber needs in order to trust
//! what it is reading.
//!
//! A subscriber that drops and re-attaches otherwise cannot tell a quiet
//! session from a lost connection, and one that finds its writes rejected
//! cannot tell why. These events close both gaps.
//!
//! `session.reconnecting` and `session.reconnected` are delivered to the
//! re-attaching subscriber alone rather than broadcast: a broadcast
//! `reconnected` would carry a sequence number *higher* than the older events
//! it introduces, which is precisely the ordering rule the rest of the system
//! is built on. They are events in shape and vocabulary, and subscription
//! notifications in delivery.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Payload of `session.reconnecting` — a subscriber is re-attaching to a
/// live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReconnecting {
    /// The sequence number the subscriber asked to resume from, or `null`
    /// when it asked for the live stream without backfill.
    pub from_seq: Option<u64>,
    /// The re-attaching subscriber.
    pub subscriber: String,
}

/// Payload of `session.reconnected` — the re-attach completed, with what the
/// subscriber did or did not get back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReconnected {
    /// What backfill delivered.
    pub replay: ReplayInfo,
}

/// The outcome of a backfill request: how much history the subscriber got
/// back, and whether any was already gone.
///
/// Three outcomes, three shapes: the requested position was still buffered
/// and everything since it was replayed; it had already been evicted and
/// nothing was; or no backfill was asked for.
///
/// The gap case is the one that matters: history is bounded, so a subscriber
/// that was away too long cannot be given what it missed. Saying so
/// explicitly — with the earliest sequence number still available, and a
/// screen snapshot where the adapter keeps one — lets the subscriber resync
/// deliberately instead of silently believing it has the whole stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayInfo {
    /// The sequence number replay actually started from, or `null` when
    /// nothing was replayed.
    pub replayed_from: Option<u64>,
    /// How many buffered events were delivered before the live stream
    /// resumed.
    pub events_replayed: u64,
    /// `true` when the requested position had already been evicted, so some
    /// events can never be delivered.
    pub gap: bool,
    /// The oldest sequence number still buffered. Present on a gap, where it
    /// tells the subscriber how much it lost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest_seq: Option<u64>,
    /// The session's reconstructed screen at re-attach time, for adapters
    /// that keep one. Present on a gap, where it is the only way to convey
    /// state the evicted events carried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_snapshot: Option<ScreenSnapshot>,
}

impl ReplayInfo {
    /// The requested position was still buffered: every event since it was
    /// replayed, and nothing was lost.
    ///
    /// One constructor per outcome, so the field combinations that describe
    /// no outcome at all — a gap that also replayed events, a replay from
    /// nowhere — are not reachable through this crate.
    pub fn within_ring(replayed_from: u64, events_replayed: u64) -> Self {
        Self {
            replayed_from: Some(replayed_from),
            events_replayed,
            gap: false,
            earliest_seq: None,
            screen_snapshot: None,
        }
    }

    /// The requested position had already been evicted: nothing was
    /// replayed, and the subscriber is attached at the live head with the
    /// events between then and now permanently unavailable.
    pub fn gap(earliest_seq: u64, screen_snapshot: Option<ScreenSnapshot>) -> Self {
        Self {
            replayed_from: None,
            events_replayed: 0,
            gap: true,
            earliest_seq: Some(earliest_seq),
            screen_snapshot,
        }
    }

    /// The subscriber asked for the live stream without backfill.
    pub fn live_from_head() -> Self {
        Self {
            replayed_from: None,
            events_replayed: 0,
            gap: false,
            earliest_seq: None,
            screen_snapshot: None,
        }
    }
}

/// The session's reconstructed screen: what a terminal attached to the CLI
/// would be showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenSnapshot {
    /// Screen width in columns.
    pub cols: u32,
    /// Screen height in rows.
    pub rows: u32,
    /// Where the cursor is.
    pub cursor: CursorPosition,
    /// Every way a cell on this screen is drawn, each listed once.
    ///
    /// Cells name a style by its position here rather than carrying one, and
    /// **index 0 is always the default style**, so reading a cell's style is
    /// one unconditional lookup: `styles[cell.style]`. A consumer that only
    /// wants the text never touches this at all.
    ///
    /// The indirection is here because it is where nearly all the size is. A
    /// terminal interface draws a whole screen out of a handful of colours —
    /// measured across the recorded sessions, four to fifteen distinct styles
    /// covering one to two thousand cells — so a style written into every
    /// cell that uses it is the same short object repeated a thousand times.
    /// Naming them instead halves a snapshot.
    pub styles: Vec<CellStyle>,
    /// The screen contents, row-major: `cells[row][col]`.
    ///
    /// There is one entry per row, always — a blank row is an empty array
    /// rather than an absent one, so a row index means the same thing on
    /// every snapshot. Within a row the trailing blank cells are dropped, so
    /// a row is at most `cols` long and usually far shorter: a column past
    /// the end of a row is blank in the default style. That is what keeps a
    /// full-screen snapshot proportional to what is written on the screen
    /// rather than to its area, which matters because a snapshot travels
    /// whole and a mostly-empty screen is the normal case.
    pub cells: Vec<Vec<ScreenCell>>,
}

/// One cell of the reconstructed screen: the character it shows, and how it
/// is drawn.
///
/// Every cell is the same shape, and the two fields that are usually
/// uninteresting are omitted when they are — a plain character in the
/// default style serializes as `{"ch": "x"}`. The alternative encodings
/// weighed against this one were a bare string per plain cell and
/// run-length rows; both are smaller, and both cost the property that makes
/// this one worth the bytes, which is that `cells[row][col]` is the cell at
/// that column and a consumer needs no second code path to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenCell {
    /// The character in the cell.
    pub ch: char,
    /// How many columns the character occupies: `1` normally, `2` for the
    /// leading half of a double-width glyph, and `0` for the column that
    /// half covers — which is carried as its own cell so that a column index
    /// still addresses a column. Omitted at the usual `1`.
    #[serde(default = "single_width", skip_serializing_if = "is_single_width")]
    pub width: u8,
    /// How the cell is drawn, as a position in the snapshot's
    /// [`styles`](ScreenSnapshot::styles). Omitted at `0`, which is the
    /// default style.
    #[serde(default, skip_serializing_if = "is_default_style")]
    pub style: u32,
}

impl ScreenCell {
    /// A cell showing `ch` in the default style, one column wide.
    pub fn plain(ch: char) -> Self {
        Self {
            ch,
            width: 1,
            style: 0,
        }
    }
}

fn single_width() -> u8 {
    1
}

fn is_default_style(style: &u32) -> bool {
    *style == 0
}

fn is_single_width(width: &u8) -> bool {
    *width == 1
}

/// The display attributes of one cell — the terminal's SGR state where it
/// was written.
///
/// Everything defaults to off, and everything off is omitted, so the common
/// cell carries no style object at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CellStyle {
    /// Text colour, or `null` for the terminal's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<CellColor>,
    /// Background colour, or `null` for the terminal's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<CellColor>,
    /// Bold, faint, or neither. One field rather than two flags because a
    /// terminal is in exactly one of the three states.
    #[serde(default, skip_serializing_if = "CellIntensity::is_normal")]
    pub intensity: CellIntensity,
    /// Italic.
    #[serde(default, skip_serializing_if = "unset")]
    pub italic: bool,
    /// Underlined.
    #[serde(default, skip_serializing_if = "unset")]
    pub underline: bool,
    /// Struck through.
    #[serde(default, skip_serializing_if = "unset")]
    pub strikethrough: bool,
    /// Blinking.
    #[serde(default, skip_serializing_if = "unset")]
    pub blink: bool,
    /// Foreground and background swapped.
    #[serde(default, skip_serializing_if = "unset")]
    pub inverse: bool,
}

impl CellStyle {
    /// Whether the cell is drawn in the terminal's default style.
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

fn unset(flag: &bool) -> bool {
    !*flag
}

/// A colour, in whichever of the two ways the CLI expressed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellColor {
    /// An index into the terminal's palette, which resolves to a colour only
    /// once a real terminal has a theme. Carried as the index rather than as
    /// the colour it would resolve to, because the runtime has no theme and
    /// substituting one would be inventing information.
    Indexed(u8),
    /// A direct colour: red, green, blue.
    Rgb([u8; 3]),
}

/// How heavily a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellIntensity {
    /// The default weight.
    #[default]
    Normal,
    /// SGR 1.
    Bold,
    /// SGR 2.
    Faint,
}

impl CellIntensity {
    /// Whether this is the default weight.
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }
}

/// A cursor position on the reconstructed screen, zero-based from the top
/// left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct CursorPosition {
    /// Row, from the top.
    pub row: u32,
    /// Column, from the left.
    pub col: u32,
}

/// Payload of `session.writer_changed` — which subscriber may write to the
/// session changed hands.
///
/// Published so every reader can reflect the current writer, including the
/// case where there now is none. The type is part of the contract from the
/// start even though nothing emits it yet: a runtime serving one caller has
/// no writer to transfer, and the callers that will need this — several
/// clients sharing one session — should be able to write against the shape
/// before it starts arriving.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWriterChanged {
    /// The subscriber that may now write, or `null` when nobody may.
    pub writer: Option<String>,
    /// The subscriber that could write before, or `null` when nobody could.
    pub previous_writer: Option<String>,
    /// Why ownership moved.
    pub reason: WriterChangeReason,
}

/// Why write ownership of a session moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriterChangeReason {
    /// A subscriber claimed it.
    Acquire,
    /// The writer gave it up.
    Release,
    /// The writer's connection went away and the runtime released it on the
    /// writer's behalf.
    TransportDrop,
}
