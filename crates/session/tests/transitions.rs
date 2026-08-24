//! 100% transition coverage, generated from the table rather than written
//! case by case: every edge in the session lifecycle diagram exercised,
//! every non-edge rejected.

use agent_bridge_session::{Edge, SessionError, SessionState, transition};

/// The session lifecycle diagram's arrow set, transcribed by hand as
/// `(from, edge, to)`.
///
/// This is deliberately a *second* copy of the table the crate ships: the
/// crate's table is what runs, this one mirrors the document, and the test
/// holds them equal — an edit to either that forgets the other fails here
/// naming the row. The three `→ Closing` arrows each carry two edges
/// (caller close and post-`Running` failure share a line in the diagram);
/// both spellings are rows because the diagram's labels name both causes.
const DIAGRAM: &[(SessionState, Edge, SessionState)] = &[
    (SessionState::Created, Edge::Launch, SessionState::Launching),
    (
        SessionState::Launching,
        Edge::PtyExecOk,
        SessionState::Connecting,
    ),
    (
        SessionState::Launching,
        Edge::LaunchFailed,
        SessionState::Closed,
    ),
    (
        SessionState::Connecting,
        Edge::FirstOutput,
        SessionState::Running,
    ),
    (
        SessionState::Connecting,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Connecting,
        Edge::ChildExitedBeforeOutput,
        SessionState::Closed,
    ),
    (
        SessionState::Running,
        Edge::ApprovalDetected,
        SessionState::AwaitingApproval,
    ),
    (
        SessionState::Running,
        Edge::Interrupt,
        SessionState::Interrupted,
    ),
    (
        SessionState::Running,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Running,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::ApprovalResolved,
        SessionState::Running,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::Interrupt,
        SessionState::Interrupted,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::Interrupted,
        Edge::Resumed,
        SessionState::Running,
    ),
    (
        SessionState::Interrupted,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Interrupted,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::Closing,
        Edge::CloseComplete,
        SessionState::Closed,
    ),
];

/// The diagram's arrows as bare `(from, to)` pairs — the topology
/// itself. 15 arrows; three carry two edge spellings each.
const DIAGRAM_ARROWS: &[(SessionState, SessionState)] = &[
    (SessionState::Created, SessionState::Launching),
    (SessionState::Launching, SessionState::Connecting),
    (SessionState::Launching, SessionState::Closed),
    (SessionState::Connecting, SessionState::Running),
    (SessionState::Connecting, SessionState::Closing),
    (SessionState::Connecting, SessionState::Closed),
    (SessionState::Running, SessionState::AwaitingApproval),
    (SessionState::Running, SessionState::Interrupted),
    (SessionState::Running, SessionState::Closing),
    (SessionState::AwaitingApproval, SessionState::Running),
    (SessionState::AwaitingApproval, SessionState::Interrupted),
    (SessionState::AwaitingApproval, SessionState::Closing),
    (SessionState::Interrupted, SessionState::Running),
    (SessionState::Interrupted, SessionState::Closing),
    (SessionState::Closing, SessionState::Closed),
];

#[test]
fn transition_matrix_full_coverage() {
    // The full product: 8 states × 12 edges = 96 pairs. Every pair is
    // either the diagram row's target or a typed rejection carrying the
    // state and the attempted edge — nothing panics, nothing defaults.
    let mut accepted = 0;
    for from in SessionState::ALL {
        for edge in Edge::ALL {
            let row = DIAGRAM
                .iter()
                .find(|(state, candidate, _)| *state == from && *candidate == edge);
            match (transition(from, edge), row) {
                (Ok(to), Some((_, _, expected))) => {
                    assert_eq!(to, *expected, "{from} --{edge:?}--> wrong target");
                    accepted += 1;
                }
                (Err(error), None) => {
                    let SessionError::InvalidStateForOperation { state, op } = error else {
                        panic!("{from} --{edge:?}--> wrong rejection type: {error}");
                    };
                    assert_eq!(state, from);
                    assert_eq!(op, edge.name());
                }
                (Ok(to), None) => panic!("{from} --{edge:?}--> {to} is not in the diagram"),
                (Err(error), Some(_)) => {
                    panic!("{from} --{edge:?}--> rejected but the diagram has it: {error}")
                }
            }
        }
    }
    assert_eq!(accepted, DIAGRAM.len(), "every diagram row was exercised");
}

#[test]
fn the_tables_topology_is_exactly_the_diagrams() {
    // Project the edge table onto bare arrows and compare as sets: an edge
    // variant that quietly opened a new state pair — or lost one — is a
    // topology change the diagram did not make.
    let mut from_table: Vec<(SessionState, SessionState)> =
        DIAGRAM.iter().map(|(from, _, to)| (*from, *to)).collect();
    from_table.sort_by_key(|(from, to)| (*from as u8, *to as u8));
    from_table.dedup();

    let mut from_diagram: Vec<(SessionState, SessionState)> = DIAGRAM_ARROWS.to_vec();
    from_diagram.sort_by_key(|(from, to)| (*from as u8, *to as u8));

    assert_eq!(from_table, from_diagram);
}

#[test]
fn closed_is_terminal_and_created_only_launches() {
    // The two ends of the machine, stated on their own because they are
    // the rows a refactor is most likely to disturb: nothing leaves
    // Closed, and the only thing Created does is launch.
    for edge in Edge::ALL {
        assert!(
            transition(SessionState::Closed, edge).is_err(),
            "Closed must be terminal, but {edge:?} left it"
        );
        let from_created = transition(SessionState::Created, edge);
        if edge == Edge::Launch {
            assert_eq!(from_created.unwrap(), SessionState::Launching);
        } else {
            assert!(from_created.is_err(), "Created --{edge:?}--> must reject");
        }
    }
}

#[test]
fn rejections_map_to_the_invalid_state_wire_code() {
    let error = transition(SessionState::Closed, Edge::Launch).unwrap_err();
    assert_eq!(error.jsonrpc_code(), -32006);
}
