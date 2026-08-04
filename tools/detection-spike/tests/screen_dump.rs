//! Tuning aid, not a test of anything: dumps every evaluation-point screen
//! of the fixtures matching `DUMP_FILTER` (a substring of the fixture id),
//! which is how the screen pattern needles and dialog anchors were read out
//! of the rendered corpus — and how the next pattern gets added. Ignored so
//! the suite never runs it; invoke explicitly:
//!
//! ```text
//! DUMP_FILTER=claude/2.1.201/clear-80x24 cargo test -p \
//!   agent-bridge-detection-spike --test screen_dump -- --ignored --nocapture
//! ```
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;

use agent_bridge_detection_spike::corpus;
use agent_bridge_detection_spike::pacing::PacedInput;
use agent_bridge_detection_spike::screen;

#[test]
#[ignore]
fn dump_screens() {
    let corpus_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let filter = std::env::var("DUMP_FILTER").unwrap_or_default();
    for cli in ["claude", "codex"] {
        let fixtures = corpus::discover(&corpus_root, &[cli.to_string()]).expect("discover");
        for fixture in fixtures {
            if !fixture.id.to_string().contains(&filter) {
                continue;
            }
            let input = PacedInput::load(&fixture.dir).expect("load");
            let points =
                screen::eval_points(&input, fixture.id.cols, fixture.id.rows).expect("replay");
            for point in points {
                println!(
                    "\n===== {} point {} ({:?}) =====",
                    fixture.id, point.ordinal, point.cause
                );
                for (index, row) in point.rows.iter().enumerate() {
                    println!("{index:3}|{row}");
                }
            }
        }
    }
}
