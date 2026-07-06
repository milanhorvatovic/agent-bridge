//! CI-matrix self-test — a placeholder so the three-OS matrix has something
//! honest to compile, test, and run before any runtime code lands. It also
//! exercises the one invariant the runtime's PTY read loop will most depend on:
//! UTF-8 that arrives split across read boundaries must be reassembled, and
//! genuinely invalid bytes must be surfaced, never silently dropped. Replaced
//! by real probes as the runtime lands.

// This crate legitimately owns stdout — the platform report *is* its output —
// so it is exempt from the workspace-wide stdout-macro ban in clippy.toml.
#![allow(clippy::disallowed_macros)]

fn platform_report() -> String {
    format!(
        "ci-selftest ok: os={} arch={} family={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )
}

/// Re-assembles a UTF-8 string from byte chunks split mid-codepoint, the way a
/// PTY read loop sees them: an incomplete suffix is carried forward across
/// reads, never dropped.
fn reassemble_split_utf8(chunks: &[&[u8]]) -> Result<String, std::string::FromUtf8Error> {
    let mut buf = Vec::new();
    for chunk in chunks {
        buf.extend_from_slice(chunk);
    }
    String::from_utf8(buf)
}

fn main() {
    // Exercise the reassembly contract in the running binary too, so the CI
    // matrix proves it on every OS at runtime, not only under `cargo test`.
    let full = "héllo 🌍".as_bytes();
    let reassembled = reassemble_split_utf8(&[&full[..2], &full[2..9], &full[9..]])
        .expect("split-codepoint reassembly must succeed");
    assert_eq!(reassembled, "héllo 🌍", "reassembled text must be lossless");
    println!("{}", platform_report());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_report_names_this_os() {
        let report = platform_report();
        assert!(
            report.contains("linux") || report.contains("macos") || report.contains("windows"),
            "unexpected platform report: {report}"
        );
    }

    #[test]
    fn utf8_survives_mid_codepoint_chunk_split() {
        // "héllo 🌍" split inside both the 2-byte 'é' and the 4-byte '🌍'.
        let full = "héllo 🌍".as_bytes();
        let chunks: Vec<&[u8]> = vec![&full[..2], &full[2..9], &full[9..]];
        assert_eq!(reassemble_split_utf8(&chunks).unwrap(), "héllo 🌍");
    }

    #[test]
    fn genuinely_invalid_utf8_is_detected_not_dropped() {
        // 0xFF can never appear in UTF-8; the reassembler must surface the
        // error rather than silently dropping the bytes.
        let chunks: Vec<&[u8]> = vec![b"ok ", &[0xFF, 0xFE], b" tail"];
        assert!(reassemble_split_utf8(&chunks).is_err());
    }
}
