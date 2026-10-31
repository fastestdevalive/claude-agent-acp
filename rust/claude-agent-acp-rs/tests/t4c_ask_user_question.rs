use claude_agent_acp_rs::agent::{session_spawn_options, ServeOptions};
use claude_agent_acp_rs::client_caps::ClientCapabilities;
use claude_agent_acp_rs::elicitation::{
    apply_ask_elicitation_response, ask_user_questions_to_create_request,
    extract_ask_user_questions, AskUserQuestionOutcome,
};
use claude_agent_acp_rs::sdk_options::ClaudeOptions;
use serde_json::json;

#[test]
fn t4c_ask_user_question_form_built_from_questions() {
    let input = json!({
        "questions": [
            {
                "question": "Which database?",
                "header": "Database",
                "multiSelect": false,
                "options": [
                    { "label": "Postgres", "description": "Relational" },
                    { "label": "Redis", "description": "Key-value" }
                ]
            }
        ]
    });
    let questions = extract_ask_user_questions(&input).expect("questions extracted");
    let request = ask_user_questions_to_create_request(&questions, "sess-1", Some("call-1"), false);

    assert_eq!(request["mode"], "form");
    assert_eq!(request["sessionId"], "sess-1");
    assert_eq!(request["toolCallId"], "call-1");
    assert_eq!(request["message"], "Which database?");

    let schema = &request["requestedSchema"]["properties"];
    assert_eq!(schema["question_0"]["title"], "Database");
    assert_eq!(schema["question_0"]["type"], "string");
    assert_eq!(schema["question_0_custom"]["title"], "Other");
}

#[test]
fn t4c_ask_user_question_air_custom_answer_companion() {
    let input = json!({
        "questions": [
            {
                "question": "Pick color",
                "options": [{ "label": "Red" }]
            }
        ]
    });
    let questions = extract_ask_user_questions(&input).expect("extracted");
    let req = ask_user_questions_to_create_request(&questions, "sess-1", None, true);

    let custom_field = &req["requestedSchema"]["properties"]["question_0_custom"];
    assert_eq!(
        custom_field["_meta"]["_askUserQuestionCustomAnswer"],
        json!({ "questionId": "question_0", "isCustomAnswer": true })
    );
    assert_eq!(
        custom_field["_meta"]["jetbrains"]["air"]["customAnswer"],
        json!({ "questionId": "question_0", "isCustomAnswer": true })
    );
}

#[test]
fn t4c_ask_user_question_capability_gates_disallowed_tools() {
    // `session_spawn_options` is the pure function that turns the capability
    // decision into the child's `--disallowedTools` (agent.rs:622-626).
    for (elicitation_form, expect_disallowed) in [(false, true), (true, false)] {
        let caps = ClientCapabilities {
            elicitation_form,
            ..Default::default()
        };
        // The gate the agent applies: AskUserQuestion stays disallowed until the
        // client can present a form elicitation.
        let ask_disallowed = !caps.elicitation_form;

        let spawn = session_spawn_options(
            &ServeOptions::default(),
            Some(std::env::temp_dir()),
            "sess-1".to_string(),
            ClaudeOptions::default(),
            "default".to_string(),
            false,
            ask_disallowed,
        );
        assert_eq!(
            spawn
                .disallowed_tools
                .iter()
                .any(|tool| tool == "AskUserQuestion"),
            expect_disallowed,
            "elicitation.form={elicitation_form} must decide whether AskUserQuestion is disallowed"
        );
    }
}

#[test]
fn t4c_ask_user_question_answers_folded_into_updated_input() {
    let input = json!({
        "questions": [
            {
                "question": "Pick framework",
                "options": [{ "label": "Actix" }, { "label": "Axum" }]
            }
        ]
    });
    let questions = extract_ask_user_questions(&input).expect("extracted");

    let response = json!({
        "action": "accept",
        "content": {
            "question_0": "Axum"
        }
    });

    let outcome = apply_ask_elicitation_response(&response, &input, &questions);
    match outcome {
        AskUserQuestionOutcome::Answered { updated_input } => {
            assert_eq!(updated_input["answers"]["Pick framework"], "Axum");
        }
        AskUserQuestionOutcome::Cancel => panic!("expected answered"),
    }
}

/// The wire shape is insertion-ordered, so compare the serialized request with
/// upstream's literal (`elicitation.ts:174-247`): `mode, sessionId, toolCallId,
/// message, requestedSchema`, each property `type, title, description, …`, and
/// the custom field `type, title, description` (no `_meta` for a non-AIR client).
#[test]
fn t4c_ask_user_question_request_key_order_matches_upstream() {
    let input = json!({
        "questions": [
            {
                "question": "Which library?",
                "header": "Library",
                "multiSelect": false,
                "options": [
                    { "label": "date-fns", "description": "Lightweight" },
                    { "label": "luxon" }
                ]
            }
        ]
    });
    let questions = extract_ask_user_questions(&input).expect("extracted");
    let request = ask_user_questions_to_create_request(&questions, "sess-1", Some("tool-1"), false);

    assert_eq!(
        serde_json::to_string(&request).expect("serialize"),
        r#"{"mode":"form","sessionId":"sess-1","toolCallId":"tool-1","message":"Which library?","requestedSchema":{"type":"object","properties":{"question_0":{"type":"string","title":"Library","oneOf":[{"const":"date-fns","title":"date-fns","description":"Lightweight"},{"const":"luxon","title":"luxon"}]},"question_0_custom":{"type":"string","title":"Other","description":"Type your own answer, or add a note to the option you chose above (optional)."}}}}"#
    );
}
