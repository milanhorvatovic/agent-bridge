use agent_bridge_stream::ScreenState;
#[test]
fn footprints() {
    for (c, r) in [
        (80u16, 24u16),
        (120, 40),
        (200, 100),
        (1, 65535),
        (15, 65535),
    ] {
        let mut s = ScreenState::new(c, r, true);
        if !s.is_kept() {
            eprintln!("{c}x{r}: refused");
            continue;
        }
        let cold = s.footprint();
        // warm the dedup window up to its full size
        for round in 0..5 {
            let mut p = String::new();
            for row in 0..r.min(2000) {
                p.push_str(&format!("\x1b[{};1Hr{round}c{row}\r\n", row + 1));
            }
            s.feed(p.as_bytes());
            s.evaluate();
        }
        eprintln!(
            "{c}x{r}: cold {:.1} KiB -> warm {:.1} KiB",
            cold as f64 / 1024.0,
            s.footprint() as f64 / 1024.0
        );
    }
}
