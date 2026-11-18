//! Headless e2e for the learnings dock count after a turn: the agent's own
//! `rlm.harness` writes become learnings without any transcript row, so the
//! settled turn itself refreshes the count.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixStream;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    attach_response, create_response, detach_response, options, run_headless, serve_connection,
    write_json, MockSupervisor, Request,
};

impl MockSupervisor {
    /// Serve one connection. `get_learnings` answers none until a prompt
    /// ran, then the one learning the turn's kernel write left behind.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let mut prompted = false;
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &attach_response(id, "learnings session"));
                }
                "get_learnings" => {
                    let learnings = if prompted {
                        vec![kernel_write_learning()]
                    } else {
                        Vec::new()
                    };
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_learnings",
                            "success": true,
                            "data": { "learnings": learnings },
                        }),
                    );
                }
                "prompt" => {
                    prompted = true;
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                    stream_turn(writer);
                }
                "detach" => {
                    write_json(writer, &detach_response(id));
                }
                _ => return false,
            }
            true
        });
    }
}

/// The review card details of one pending learning a kernel write recorded.
fn kernel_write_learning() -> Value {
    json!({
        "refinementId": "refine_1",
        "summary": "Agent created memory Stack probe",
        "scope": "local",
        "edits": [{"action": "create", "kind": "memory", "id": "m1", "applied": true}],
        "reviewState": "pending",
        "rationale": "Direct kernel write (rlm.harness).",
    })
}

/// One assistant turn with no refinement row.
fn stream_turn(writer: &mut UnixStream) {
    let event = |payload: Value| json!({ "type": "session_event", "activeSessionId": "s1", "event": payload });
    write_json(writer, &event(json!({ "type": "turn_start" })));
    write_json(
        writer,
        &event(json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "done" }],
                "stopReason": "stop",
            },
        })),
    );
    write_json(writer, &event(json!({ "type": "turn_end" })));
    write_json(writer, &event(json!({ "type": "agent_end" })));
}

#[test]
fn a_settled_turn_refreshes_the_learnings_count() {
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "\u{270e} 0 learnings".to_string(),
            timeout_ms: 5_000,
        },
        HeadlessStep::Submit("remember this".to_string()),
        HeadlessStep::WaitIdle { timeout_ms: 10_000 },
        HeadlessStep::WaitRender {
            needle: "\u{270e} 1 learning".to_string(),
            timeout_ms: 5_000,
        },
    ];
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve()
        },
    );
    let last = outcome.frames.last().cloned().unwrap_or_default();
    assert!(
        last.contains("\u{270e} 1 learning"),
        "the settled turn refreshed the dock's learnings count:\n{}",
        outcome.frames.join("\n")
    );
}
