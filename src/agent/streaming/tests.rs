use super::*;
use serde_json::{Value, json};

fn frame(delta: Value) -> String {
    json!({"choices":[{"index":0,"delta":delta}]}).to_string()
}
fn terminal(reason: &str) -> String {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}).to_string()
}
fn complete(mut state: Accumulator, reason: &str) -> accumulator::StreamCompletion {
    state.push(&terminal(reason)).unwrap();
    state.push("[DONE]").unwrap();
    state.finish().unwrap()
}

#[test]
fn assembled_message_is_independent_of_every_wire_split() {
    let wire = format!(
        ": heartbeat\r\ndata: {}\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        frame(json!({"role":"assistant","content":"Привет"})),
        frame(json!({"content":"🙂\n"})),
        terminal("stop")
    );
    for split in 0..=wire.len() {
        let mut decoder = Decoder::default();
        let mut state = Accumulator::default();
        let mut visible = String::new();
        for bytes in [&wire.as_bytes()[..split], &wire.as_bytes()[split..]] {
            for event in decoder.push(bytes).unwrap() {
                visible.push_str(&state.push(&event).unwrap().content);
            }
        }
        let completed = state.finish().unwrap();
        assert_eq!(visible, "Привет🙂\n");
        assert_eq!(
            completed
                .trajectory
                .message()
                .unwrap()
                .content_text()
                .unwrap(),
            visible
        );
        assert_eq!(completed.finish_reason, "stop");
        assert!(!completed.truncated);
    }
}

#[test]
fn interleaved_tools_preserve_indices_and_late_identity_fields() {
    let mut state = Accumulator::default();
    state
        .push(&frame(json!({"role":"assistant","tool_calls":[
            {"index":1,"function":{"name":"write_","arguments":"{\"p"}},
            {"index":0,"function":{"name":"ba","arguments":"{"}}
        ]})))
        .unwrap();
    assert!(state.semantic_started);
    state.push(&frame(json!({"tool_calls":[
        {"index":0,"id":"first","type":"function","function":{"name":"sh","arguments":"\"command\":\"ls\"}"}},
        {"index":1,"id":"second","type":"function","function":{"name":"file","arguments":"ath\":\"a\"}"}}
    ]}))).unwrap();
    let result = complete(state, "tool_calls");
    let tools = result
        .trajectory
        .message()
        .unwrap()
        .tool_calls
        .as_ref()
        .unwrap();
    assert_eq!(tools[0].id, "first");
    assert_eq!(tools[0].function.name, "bash");
    assert_eq!(tools[0].function.arguments, "{\"command\":\"ls\"}");
    assert_eq!(tools[1].id, "second");
    assert_eq!(tools[1].function.name, "write_file");
}

#[test]
fn reasoning_representation_is_selected_once_and_opaque_data_round_trips() {
    for string_first in [true, false] {
        let mut state = Accumulator::default();
        let first = state.push(&frame(json!({"role":"assistant", "reasoning_details":[
            {"index":2,"type":"reasoning.encrypted","data":"opaque","id":"enc","format":"provider","extension":{"signature":1}}
        ]}))).unwrap();
        assert!(first.reasoning.is_empty());
        assert!(state.semantic_started);
        let value = if string_first {
            json!("same ")
        } else {
            Value::Null
        };
        let first = state
            .push(&frame(json!({"reasoning":value,"reasoning_details":[
                {"index":0,"id":null,"type":"reasoning.text","text":"same ","signature":null}
            ]})))
            .unwrap();
        let second = state.push(&frame(json!({"content":"answer", "reasoning":"twice","reasoning_details":[
            {"index":0,"id":"late-id","type":"reasoning.text","text":"twice","signature":"sig"},
            {"id":"enc","type":"reasoning.encrypted","data":"-tail"}
        ]}))).unwrap();
        assert_eq!(first.reasoning + &second.reasoning, "same twice");
        let result = complete(state, "stop");
        let message = result.trajectory.message().unwrap();
        let details = message.reasoning_details.as_ref().unwrap();
        assert_eq!(details[0]["data"], "opaque-tail");
        assert_eq!(details[0]["extension"], json!({"signature":1}));
        assert_eq!(details[1]["text"], "same twice");
        assert_eq!(details[1]["signature"], "sig");
        assert_eq!(details[1]["id"], "late-id");
        let snapshot = serde_json::to_value(&result.trajectory).unwrap();
        let resumed: super::super::messages::TrajectoryMessage =
            serde_json::from_value(snapshot.clone()).unwrap();
        assert_eq!(serde_json::to_value(resumed).unwrap(), snapshot);
    }
}

#[test]
fn summary_and_anonymous_details_are_retained_in_arrival_order() {
    let mut state = Accumulator::default();
    let delta = state.push(&frame(json!({"role":"assistant","content":"answer","reasoning_details":[
        {"type":"reasoning.summary","summary":"one "}, {"type":"reasoning.summary","summary":"two"}
    ]}))).unwrap();
    assert_eq!(delta.reasoning, "one two");
    let result = complete(state, "stop");
    assert_eq!(
        result
            .trajectory
            .message()
            .unwrap()
            .reasoning_details
            .as_ref()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn reasoning_text_before_type_is_displayed_once_when_identity_is_completed() {
    let mut state = Accumulator::default();
    let early = state
        .push(&frame(json!({"role":"assistant","reasoning_details":[
            {"index":0,"text":"Ранний "}, {"index":1,"data":"SECRET"}
        ]})))
        .unwrap();
    assert!(early.reasoning.is_empty());
    let typed = state
        .push(&frame(json!({"reasoning_details":[
            {"index":0,"id":"late","type":"reasoning.text","text":"текст"},
            {"index":1,"type":"reasoning.encrypted"}
        ]})))
        .unwrap();
    assert_eq!(typed.reasoning, "Ранний текст");
    let signature = state
        .push(&frame(json!({"content":"answer", "reasoning_details":[
            {"id":"late","signature":"opaque-signature"}
        ]})))
        .unwrap();
    assert!(signature.reasoning.is_empty());
    let result = complete(state, "stop");
    assert_eq!(
        result.trajectory.message().unwrap().reasoning_text(),
        "Ранний текст"
    );
}

#[test]
fn usage_after_terminal_reason_replaces_snapshot_and_accepts_empty_choices() {
    let mut state = Accumulator::default();
    state
        .push(&frame(json!({"role":"assistant","content":"answer"})))
        .unwrap();
    state.push(&terminal("stop")).unwrap();
    state
        .push(&json!({"choices":[],"usage":{"completion_tokens":1}}).to_string())
        .unwrap();
    state.push(&json!({"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"completion_tokens":2}}).to_string()).unwrap();
    state.push("[DONE]").unwrap();
    assert_eq!(
        state
            .finish()
            .unwrap()
            .trajectory
            .usage()
            .unwrap()
            .completion_tokens,
        Some(2)
    );
}

#[test]
fn ignores_other_choices_and_role_only_is_not_semantic() {
    let mut state = Accumulator::default();
    state.push(&json!({"choices":[{"index":1,"delta":{"content":"ignored"}},{"index":0,"delta":{"role":"assistant","content":null}}]}).to_string()).unwrap();
    assert!(!state.semantic_started);
    state.push(&frame(json!({"content":"selected"}))).unwrap();
    assert_eq!(
        complete(state, "stop")
            .trajectory
            .message()
            .unwrap()
            .content_text()
            .unwrap(),
        "selected"
    );
}

#[test]
fn rejects_early_eof_missing_role_terminal_reason_and_semantic_data_after_finish() {
    for missing in ["done", "role", "reason", "content"] {
        let mut state = Accumulator::default();
        if missing != "role" {
            state.push(&frame(json!({"role":"assistant"}))).unwrap();
        }
        if missing != "content" {
            state.push(&frame(json!({"content":"answer"}))).unwrap();
        }
        if missing != "reason" {
            state.push(&terminal("stop")).unwrap();
        }
        if missing != "done" {
            state.push("[DONE]").unwrap();
        }
        assert!(state.finish().is_err(), "{missing}");
    }
    let mut state = Accumulator::default();
    state.push(&terminal("stop")).unwrap();
    assert!(state.push(&frame(json!({"content":"late"}))).is_err());
    let mut state = Accumulator::default();
    state.push("[DONE]").unwrap();
    assert!(state.push(&terminal("stop")).is_err());
}

#[test]
fn detects_provider_errors_malformed_json_conflicting_roles_and_reasons() {
    let error = Accumulator::default()
        .push(r#"{"error":{"code":503,"message":"unavailable"}}"#)
        .unwrap_err();
    assert!(error.retryable);
    assert_eq!(error.to_string(), "unavailable");
    assert!(!Accumulator::default().push("{").unwrap_err().retryable);
    assert!(
        !Accumulator::default()
            .push(&frame(json!({"role":"user"})))
            .unwrap_err()
            .retryable
    );
    assert!(
        Accumulator::default()
            .push(&terminal("error"))
            .unwrap_err()
            .retryable
    );
    assert!(
        !Accumulator::default()
            .push(&terminal("unsupported"))
            .unwrap_err()
            .retryable
    );
    let mut state = Accumulator::default();
    let error = state.push(&json!({"error":{"code":"server_error"},"choices":[{"index":0,"delta":{"reasoning_details":[{"data":"opaque"}]}}]}).to_string()).unwrap_err();
    assert!(error.retryable && state.semantic_started);
    let mut state = Accumulator::default();
    state.push(&terminal("stop")).unwrap();
    assert!(state.push(&terminal("tool_calls")).is_err());
}

#[test]
fn incomplete_duplicate_conflicting_and_truncated_tools_never_validate() {
    for (reason, tools) in [
        (
            "tool_calls",
            json!([{"index":0,"id":"x","type":"function","function":{"name":"bash","arguments":"{"}}]),
        ),
        (
            "tool_calls",
            json!([{"index":0,"function":{"arguments":"{}"}}]),
        ),
        (
            "tool_calls",
            json!([{"index":0,"id":"x","type":"function","function":{"name":"bash","arguments":"{}"}}, {"index":1,"id":"x","type":"function","function":{"name":"bash","arguments":"{}"}}]),
        ),
        (
            "length",
            json!([{"index":0,"id":"x","type":"function","function":{"name":"bash","arguments":"{}"}}]),
        ),
        (
            "stop",
            json!([{"index":0,"id":"x","type":"function","function":{"name":"bash","arguments":"{}"}}]),
        ),
    ] {
        let mut state = Accumulator::default();
        state
            .push(&frame(json!({"role":"assistant","tool_calls":tools})))
            .unwrap();
        state.push(&terminal(reason)).unwrap();
        state.push("[DONE]").unwrap();
        assert!(state.finish().is_err());
    }
    let mut state = Accumulator::default();
    state
        .push(&frame(json!({"tool_calls":[{"index":0,"id":"a"}]})))
        .unwrap();
    assert!(
        state
            .push(&frame(json!({"tool_calls":[{"index":0,"id":"b"}]})))
            .is_err()
    );
}

#[test]
fn length_text_is_kept_with_explicit_truncation_and_missing_usage_stays_unknown() {
    let mut state = Accumulator::default();
    state
        .push(&frame(json!({"role":"assistant","content":"prefix"})))
        .unwrap();
    let result = complete(state, "length");
    assert!(result.truncated);
    assert!(result.trajectory.usage().is_none());
}

#[test]
fn response_budget_is_checked_before_growth_and_reasoning_conflicts_fail() {
    let mut budget = Budget {
        used: MAX_RESPONSE_BYTES - 2,
    };
    assert!(budget.reserve(3).is_err());
    assert_eq!(budget.used, MAX_RESPONSE_BYTES - 2);
    let mut details = reasoning::Details::default();
    let mut budget = Budget::default();
    details
        .append(
            vec![json!({"index":0,"type":"reasoning.text","text":"x"})],
            &mut budget,
        )
        .unwrap();
    assert!(
        details
            .append(
                vec![json!({"index":0,"type":"reasoning.encrypted","data":"y"})],
                &mut budget
            )
            .is_err()
    );
}
