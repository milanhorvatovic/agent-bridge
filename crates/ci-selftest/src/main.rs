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

/// The reassembler's error: the byte stream contained (or ended inside) a
/// sequence that can never become valid UTF-8, which must be reported rather
/// than silently dropped.
#[derive(Debug, PartialEq)]
struct InvalidUtf8;

/// Re-assembles a UTF-8 string from byte chunks split mid-codepoint, the way a
/// PTY read loop sees them: each chunk is decoded as it arrives, an incomplete
/// trailing codepoint is carried forward into the next read, and only
/// genuinely invalid bytes — including a stream that ends mid-codepoint — are
/// an error, never dropped.
fn reassemble_split_utf8(chunks: &[&[u8]]) -> Result<String, InvalidUtf8> {
    let mut out = String::new();
    let mut carry: Vec<u8> = Vec::new();
    for chunk in chunks {
        carry.extend_from_slice(chunk);
        match std::str::from_utf8(&carry) {
            Ok(text) => {
                out.push_str(text);
                carry.clear();
            }
            Err(err) => {
                let valid = err.valid_up_to();
                // Unreachable panic: `valid_up_to` guarantees the prefix is
                // valid UTF-8.
                out.push_str(std::str::from_utf8(&carry[..valid]).unwrap());
                match err.error_len() {
                    // The suffix is not wrong, just not complete yet — carry
                    // it into the next chunk.
                    None => {
                        carry.drain(..valid);
                    }
                    // No continuation could ever repair these bytes.
                    Some(_) => return Err(InvalidUtf8),
                }
            }
        }
    }
    // A stream that ends inside a codepoint is truncated output: report it,
    // never swallow the carried bytes.
    if carry.is_empty() {
        Ok(out)
    } else {
        Err(InvalidUtf8)
    }
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
        assert_eq!(reassemble_split_utf8(&chunks), Err(InvalidUtf8));
    }

    #[test]
    fn truncated_final_codepoint_is_an_error_not_dropped() {
        // A stream ending mid-codepoint must not silently lose the carried
        // suffix: end-of-stream turns "incomplete" into "invalid".
        let full = "héllo 🌍".as_bytes();
        let chunks: Vec<&[u8]> = vec![&full[..2], &full[2..full.len() - 1]];
        assert_eq!(reassemble_split_utf8(&chunks), Err(InvalidUtf8));
    }
}
