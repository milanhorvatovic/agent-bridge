//! The matcher engine fed the way the runtime will feed it: reader-shaped
//! chunks through the stripper, segmented into lines, against the real
//! fake-CLI pattern pack.
//!
//! Unit tests hand the engine clean lines; a session will not. Output
//! arrives in chunks cut wherever the terminal read happened to end —
//! mid-word, mid-escape-sequence, between a CR and its LF — decorated with
//! the control sequences a real CLI paints with. These tests hold the whole
//! text path to the contract the pieces claim individually: what reaches
//! the matchers is the stripped, line-segmented stream, and chunk
//! boundaries change nothing.

use agent_bridge_events::{EventBody, EventKind};
use agent_bridge_pty::{EndOfStream, ReadChunk};
use agent_bridge_stream::{
    LineAssembler, MatcherEngine, ReaderConfig, ReaderOutputs, SessionMatcherState, StreamReader,
    Stripper, load_dir,
};

/// The committed pack under test — the same files an adapter registration
/// would embed.
fn fake_cli_pack() -> MatcherEngine {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../patterns/fake-cli/1.0");
    MatcherEngine::builder()
        .records(load_dir(&dir).expect("the committed fake-cli pack loads"))
        .compile()
        .expect("the committed fake-cli pack compiles")
}

/// Stripper → segmentation → engine, per decoded text chunk: the text path
/// as the per-session pipeline will run it.
struct TextPath {
    stripper: Stripper,
    assembler: LineAssembler,
    session: SessionMatcherState,
}

impl TextPath {
    fn new(engine: &MatcherEngine) -> Self {
        Self {
            stripper: Stripper::new(),
            assembler: LineAssembler::new(),
            session: engine.new_session(),
        }
    }

    fn feed(&mut self, engine: &MatcherEngine, chunk: &str) -> Vec<EventBody> {
        let stripped = self.stripper.feed(chunk);
        let mut events = Vec::new();
        for line in self.assembler.push(&stripped.text) {
            events.extend(engine.evaluate_line(&mut self.session, &line));
        }
        events
    }

    /// An evaluation point: the pending, unterminated tail gets its look.
    fn quiet(&mut self, engine: &MatcherEngine) -> Vec<EventBody> {
        engine.evaluate_pending(&mut self.session, self.assembler.pending())
    }
}

fn approval_of(events: &[EventBody]) -> (&str, String) {
    let event = events
        .iter()
        .find(|event| matches!(event.kind, EventKind::PromptApprovalRequired(_)))
        .expect("an approval event");
    let EventKind::PromptApprovalRequired(payload) = &event.kind else {
        unreachable!()
    };
    (
        payload.prompt.as_str(),
        event
            .approval_id
            .clone()
            .expect("approval_id is required on this event"),
    )
}

/// The milestone row: the approval fixture line, arriving styled and split,
/// produces `prompt.approval_required` with a valid generated id.
#[test]
fn fake_cli_approval_emits_approval_required() {
    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);

    // Bold-styled prompt, split mid-escape-sequence and mid-word across
    // three reads, ending in CRLF: every seam the pipeline claims to
    // tolerate, at once.
    let mut events = Vec::new();
    for chunk in [
        "\u{1b}[",
        "1mAllow filesystem",
        " write? [y/N]\u{1b}[0m\r",
        "\n",
    ] {
        events.extend(path.feed(&engine, chunk));
    }
    assert_eq!(events.len(), 1);
    let (prompt, approval_id) = approval_of(&events);
    assert_eq!(prompt, "Allow filesystem write?");
    uuid::Uuid::parse_str(&approval_id).expect("a generated v4 id");

    let EventKind::PromptApprovalRequired(payload) = &events[0].kind else {
        unreachable!()
    };
    assert_eq!(
        payload.options,
        Some(vec!["y".to_string(), "n".to_string()])
    );
}

#[test]
fn fake_cli_tool_marker_emits_call_started() {
    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);

    let events = path.feed(&engine, "{{tool: bash, cmd: git status}}\n");
    let EventKind::ToolCallStarted(payload) = &events[0].kind else {
        panic!("expected tool.call_started, got {:?}", events[0].kind);
    };
    assert_eq!(payload.tool, "bash");
    assert_eq!(payload.command.as_deref(), Some("git status"));
    uuid::Uuid::parse_str(&payload.call_id).expect("a generated v4 id");

    // Planted mid-line, the anchored marker is not a tool call.
    let spoofed = path.feed(
        &engine,
        "narrating {{tool: bash, cmd: rm -rf /}} casually\n",
    );
    assert!(
        !spoofed
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolCallStarted(_))),
        "an anchored marker inside a token stream must not fire"
    );
}

/// A prompt wording the pack does not know degrades to unrecognized
/// output — the resilience event — and ordinary output stays silent.
#[test]
fn unmatched_prompt_shape_degrades_through_the_pipeline() {
    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);

    let unknown = path.feed(&engine, "\u{1b}[33mDelete everything? (yes/no)\u{1b}[0m\n");
    assert!(matches!(
        &unknown[0].kind,
        EventKind::StreamUnrecognizedOutput(payload)
            if payload.content == "Delete everything? (yes/no)"
    ));

    assert!(
        path.feed(&engine, "ordinary token output with no question\n")
            .is_empty()
    );
}

/// A prompt that never ends its line — the shape a real prompt is — fires
/// at the evaluation point from the pending tail.
#[test]
fn an_unterminated_prompt_fires_at_the_evaluation_point() {
    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);

    assert!(
        path.feed(&engine, "Allow secret exfiltration? [y/N]")
            .is_empty()
    );
    let at_quiet = path.quiet(&engine);
    let (prompt, _) = approval_of(&at_quiet);
    assert_eq!(prompt, "Allow secret exfiltration?");

    // The unchanged tail at the next quiet period stays reported-once.
    assert!(path.quiet(&engine).is_empty());
}

/// The cold-start `> ` prompt is not in the pack: unrecognized, not
/// silent, once.
#[test]
fn an_unknown_pending_prompt_degrades_once() {
    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);

    assert!(path.feed(&engine, "fake-cli: session ready\n> ").is_empty());
    let at_quiet = path.quiet(&engine);
    assert!(matches!(
        &at_quiet[0].kind,
        EventKind::StreamUnrecognizedOutput(payload) if payload.content == ">"
    ));
    assert!(path.quiet(&engine).is_empty(), "same tail, no repeat");
}

/// The same approval fixture, driven through the per-session reader — the
/// full 1.4 → 1.5 → matcher path, chunk boundaries chosen by the reader.
#[tokio::test]
async fn the_reader_fed_text_path_detects_the_approval() {
    struct Scripted(std::collections::VecDeque<ReadChunk>);
    impl agent_bridge_stream::ChunkSource for Scripted {
        async fn next(&mut self) -> Option<ReadChunk> {
            self.0.pop_front()
        }
    }

    let chunks = std::collections::VecDeque::from([
        ReadChunk::Output(b"\x1b[1mAllow file".to_vec()),
        ReadChunk::Output(b"system write? [y/N]\x1b[0m\n".to_vec()),
        ReadChunk::End(EndOfStream::Eof),
    ]);

    let (text_sender, mut text_receiver) = tokio::sync::mpsc::channel(16);
    let (incident_sender, _incident_receiver) = tokio::sync::mpsc::channel(16);
    let reader = StreamReader::new(
        ReaderConfig::default(),
        ReaderOutputs {
            text: text_sender,
            vt: None,
            incidents: incident_sender,
        },
    );
    let report = reader.run(Scripted(chunks)).await;
    assert!(matches!(
        report.end,
        agent_bridge_stream::ReaderEnd::Stream(EndOfStream::Eof)
    ));

    let engine = fake_cli_pack();
    let mut path = TextPath::new(&engine);
    let mut events = Vec::new();
    while let Some(chunk) = text_receiver.recv().await {
        events.extend(path.feed(&engine, &chunk));
    }
    let (prompt, _) = approval_of(&events);
    assert_eq!(prompt, "Allow filesystem write?");
}
