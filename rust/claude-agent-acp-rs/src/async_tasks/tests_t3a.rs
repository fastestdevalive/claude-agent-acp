//! Turn 3 / lane A (F15): the background-command half of the async-task runtime
//! (`src/tests/async-tasks.test.ts`, 0.94.0): promotion by a structured Bash
//! result, the terminal tombstone, the output path read from tool-result text,
//! deferred updates of a held task, and the hidden Monitor tasks.

use super::*;

fn kinds(updates: &[Value]) -> Vec<&str> {
    updates
        .iter()
        .map(|update| update["sessionUpdate"].as_str().unwrap_or(""))
        .collect()
}

fn bash_tools(command: &str) -> impl Fn(&str) -> Option<Value> + '_ {
    move |id| (id == "bash-tool").then(|| json!({"name": "Bash", "input": {"command": command}}))
}

/// The four texts by which Claude Code 2.1.287 reports a background command.
fn background_texts(id: &str, path: &str) -> Vec<String> {
    vec![
        format!("Command was manually backgrounded by user with ID: {id}. Output is being written to: {path}."),
        format!("Command was moved to the background (ID: {id}) so that a message could be sent. Output is being written to: {path}."),
        format!("Command did not complete within its 120s timeout and was moved to the background (ID: {id}). Output is being written to: {path}."),
        format!("Command running in background with ID: {id}. Output is being written to: {path}. You will be notified when it completes."),
    ]
}

fn result_block(text: &str) -> Value {
    json!([{"type": "tool_result", "tool_use_id": "bash", "content": text}])
}

fn started(background: bool) -> Value {
    json!({"task_id": "bq7x", "task_type": "local_bash", "description": "npm test",
        "tool_use_id": "bash", "is_backgrounded": background})
}

/// T-96 — a background Bash task is recovered from the structured result; the SDK
/// can have reported the `local_bash` task before, and the result promotes it.
#[test]
fn t3a_structured_bash_result_recovers_and_promotes_the_task() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let content = json!([{
        "type": "tool_result", "tool_use_id": "bash-tool",
        "content": "Command running in background with ID: bpux8xmfg. Output is being written to: /private/tmp/claude/tasks/bpux8xmfg.output. You will be notified when it completes.",
    }]);
    let tools = |id: &str| {
        (id == "bash-tool").then(|| {
            json!({"name": "Bash", "input": {"command": "npm run build", "run_in_background": true}})
        })
    };
    let task = background_bash_task_from_tool_result(
        &content,
        Some(&json!({"backgroundTaskId": "bpux8xmfg", "stdout": "", "stderr": ""})),
        &tools,
    )
    .expect("a task");
    assert_eq!(
        task,
        json!({
            "taskId": "bpux8xmfg", "taskType": "local_bash", "description": "npm run build",
            "isBackgrounded": true,
            "outputFilePath": "/private/tmp/claude/tasks/bpux8xmfg.output",
            "toolCallId": "bash-tool",
        })
    );
    let mut published = runtime.task_started(
        &json!({"taskId": "bpux8xmfg", "taskType": "local_bash", "description": "Shell"}),
    );
    published.extend(runtime.task_backgrounded(&task));
    published.extend(runtime.task_notification(
        &json!({"taskId": "bpux8xmfg", "status": "completed", "summary": "Build finished"}),
    ));
    assert_eq!(
        kinds(&published),
        ["async_task_spawned", "async_task_state_update"]
    );
    assert_eq!(published[0]["name"], "npm run build");
    assert_eq!(published[0]["taskType"], "shell");
    assert_eq!(published[0]["showInTranscript"], true);
    assert_eq!(published[0]["toolCallId"], "bash-tool");
    assert_eq!(published[1]["state"], "completed");
}

/// T-96 — the result of a batched message: the Bash result among others is the
/// one the structured result names; its path comes from the text blocks joined.
#[test]
fn t3a_batched_message_correlates_the_bash_result() {
    let content = json!([
        {"type": "tool_result", "tool_use_id": "read", "content": "package"},
        {"type": "tool_result", "tool_use_id": "bash", "content": [
            {"type": "text", "text": "Command running. Output is being written to: "},
            {"type": "text", "text": "/private/tmp/claude/tasks/task-1.output. You will be notified when done."},
        ]},
    ]);
    let tools = |id: &str| match id {
        "read" => Some(json!({"name": "Read", "input": {"file_path": "package.json"}})),
        "bash" => Some(json!({"name": "Bash", "input": {"command": "npm run build"}})),
        _ => None,
    };
    let task = background_bash_task_from_tool_result(
        &content,
        Some(&json!({"backgroundTaskId": "task-1"})),
        &tools,
    )
    .expect("a task");
    assert_eq!(task["taskId"], "task-1");
    assert_eq!(task["toolCallId"], "bash");
    assert_eq!(task["description"], "npm run build");
    assert_eq!(
        task["outputFilePath"],
        "/private/tmp/claude/tasks/task-1.output"
    );
}

/// T-96 — no task without an unambiguous Bash result, and a structured result
/// naming two tasks, no Bash result, or a path of another task gives none (or no
/// path).
#[test]
fn t3a_no_task_without_an_unambiguous_bash_result() {
    let structured = json!({"backgroundTaskId": "task-1"});
    let bash = bash_tools("npm run build");
    let two = |id: &str| match id {
        "bash-1" => Some(json!({"name": "Bash", "input": {"command": "first"}})),
        "bash-2" => Some(json!({"name": "Bash", "input": {"command": "second"}})),
        _ => None,
    };
    assert!(background_bash_task_from_tool_result(&json!([]), Some(&structured), &bash).is_none());
    assert!(background_bash_task_from_tool_result(
        &json!([{"type": "tool_result", "tool_use_id": "bash-1"}, {"type": "tool_result", "tool_use_id": "bash-2"}]),
        Some(&structured),
        &two,
    )
    .is_none());
    let read = |id: &str| (id == "read").then(|| json!({"name": "Read", "input": {}}));
    assert!(background_bash_task_from_tool_result(
        &json!([{"type": "tool_result", "tool_use_id": "read"}]),
        Some(&structured),
        &read,
    )
    .is_none());
    let other_path = json!([{"type": "tool_result", "tool_use_id": "bash-tool",
        "content": "Output is being written to: /private/tmp/claude/tasks/not-task-1.output. You will be notified"}]);
    let task = background_bash_task_from_tool_result(&other_path, Some(&structured), &bash)
        .expect("a task");
    assert!(
        task.get("outputFilePath").is_none(),
        "a path of another task"
    );
    assert!(background_bash_task_from_tool_result(
        &json!([{"type": "tool_result", "tool_use_id": "bash-tool"}]),
        Some(&json!([{"backgroundTaskId": "task-1"}, {"backgroundTaskId": "task-2"}])),
        &bash,
    )
    .is_none());
}

/// T-96 — a structured result that names its tool call disambiguates batched
/// Bash results; one naming a tool that is not Bash gives nothing.
#[test]
fn t3a_structured_tool_id_disambiguates_batched_bash_results() {
    let tools = |id: &str| match id {
        "bash-1" => Some(json!({"name": "Bash", "input": {"command": "first"}})),
        "bash-2" => Some(json!({"name": "Bash", "input": {"command": "second"}})),
        "read" => Some(json!({"name": "Read", "input": {}})),
        _ => None,
    };
    let content = json!([
        {"type": "tool_result", "tool_use_id": "bash-1", "content": "first"},
        {"type": "tool_result", "tool_use_id": "bash-2", "content": "second"},
    ]);
    let task = background_bash_task_from_tool_result(
        &content,
        Some(&json!({"tool_use_id": "bash-2", "background_task_id": "task-2"})),
        &tools,
    )
    .expect("a task");
    assert_eq!(task["taskId"], "task-2");
    assert_eq!(task["toolCallId"], "bash-2");
    assert_eq!(task["description"], "second");
    assert!(background_bash_task_from_tool_result(
        &json!([{"type": "tool_result", "tool_use_id": "bash-1", "content": "first"}]),
        Some(&json!({"tool_use_id": "read", "background_task_id": "task-2"})),
        &tools,
    )
    .is_none());
}

/// T-96 — the text of a foreground tool result creates no task, however much it
/// looks like the background announcement.
#[test]
fn t3a_foreground_result_text_creates_no_task() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let tools = bash_tools("sed -n 1,40p src/async-tasks.ts");
    let mut texts = background_texts("bg1", "/tmp/tasks/bg1.output");
    texts.extend([
        "const marker = \"Command running in background with ID: \";\n  const start = text"
            .to_string(),
        "Command running in background with ID: You will be notified when it.".to_string(),
        "Command running in background with ID: ${e}. Output is being written to: ${Sp(e)}."
            .to_string(),
    ]);
    let mut published = Vec::new();
    for text in texts {
        let content = json!([{"type": "tool_result", "tool_use_id": "bash-tool", "content": text}]);
        assert!(background_bash_task_from_tool_result(&content, None, &tools).is_none());
        published.extend(runtime.tool_results(&content));
    }
    published.extend(runtime.release_held());
    assert!(published.is_empty());
    assert!(!runtime.is_backgrounded_tool_call("bash-tool"));
}

/// T-95 — the path of every announcement format is taken when the tool result
/// comes before the task, and arrives as a progress beat when it comes after.
#[test]
fn t3a_output_path_from_the_tool_result_in_either_order() {
    let path = "/private/tmp/claude-501/project/session/tasks/bq7x.output";
    for text in background_texts("bq7x", path) {
        let mut runtime = AsyncTaskRuntime::new(true);
        let mut published = runtime.tool_results(&result_block(&text));
        published.extend(runtime.task_started(&started(true)));
        assert_eq!(published[0]["sessionUpdate"], "async_task_spawned");
        assert_eq!(published[0]["outputFilePath"], path, "{text}");

        // The order of a command that hits its timeout: the task goes to the
        // background first, the result brings the path.
        let mut runtime = AsyncTaskRuntime::new(true);
        let mut published = runtime.task_started(&started(false));
        published.extend(runtime.task_updated("bq7x", &json!({"is_backgrounded": true})));
        assert!(runtime.is_backgrounded_tool_call("bash"));
        published.extend(runtime.tool_results(&result_block(&text)));
        published.extend(runtime.task_notification(
            &json!({"task_id": "bq7x", "status": "completed", "tool_use_id": "bash"}),
        ));
        assert_eq!(published.len(), 3, "{published:?}");
        assert!(published[0].get("outputFilePath").is_none());
        assert_eq!(
            published[1],
            json!({"sessionUpdate": "async_task_progress", "asyncTaskId": "bq7x", "outputFilePath": path})
        );
        assert_eq!(
            published[2],
            json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": "bq7x", "state": "completed"})
        );
    }
}

/// T-95 — a path may hold spaces and a drive, and the sentence period after it is
/// not part of it.
#[test]
fn t3a_output_path_with_spaces() {
    let windows = "C:\\Users\\John Doe\\AppData\\Local\\Temp\\claude-0\\C--work-my-repo\\5f0c2a1e-session\\tasks\\bq7x.output";
    let posix = "/Users/John Doe/Library/Caches/claude-501/my repo/session/tasks/bq7x.output";
    let mut cases: Vec<(String, String)> = Vec::new();
    for path in [windows, posix] {
        for text in background_texts("bq7x", path) {
            cases.push((text, path.to_string()));
        }
    }
    cases.push((
        "Output is being written to: C:/Users/John Doe/Temp/tasks/bq7x.output.".into(),
        "C:/Users/John Doe/Temp/tasks/bq7x.output".into(),
    ));
    cases.push((
        "Output is being written to: \\\\server\\share\\my tasks\\tasks\\bq7x.output".into(),
        "\\\\server\\share\\my tasks\\tasks\\bq7x.output".into(),
    ));
    for (text, expected) in cases {
        let mut runtime = AsyncTaskRuntime::new(true);
        let mut published = runtime.tool_results(&result_block(&text));
        published.extend(runtime.task_started(&started(true)));
        assert_eq!(published[0]["outputFilePath"], expected, "{text}");

        let mut runtime = AsyncTaskRuntime::new(true);
        let mut published = runtime.task_started(&started(true));
        published.extend(runtime.tool_results(&result_block(&text)));
        assert_eq!(
            published[1],
            json!({"sessionUpdate": "async_task_progress", "asyncTaskId": "bq7x", "outputFilePath": expected}),
            "{text}"
        );
    }
}

/// T-95 — paths that do not count: another task's, another tool call's, a longer
/// extension or file after the suffix, no ": " boundary, a relative path, one
/// across a line, and an id that only ends the same.
#[test]
fn t3a_output_path_that_does_not_count() {
    let cases = [
        (
            "bash",
            background_texts("other", "/tmp/tasks/other.output")[3].clone(),
        ),
        (
            "read",
            "Output is being written to: /tmp/tasks/bq7x.output.".to_string(),
        ),
        (
            "bash",
            "Output is being written to: /tmp/tasks/bq7x.outputs.".to_string(),
        ),
        (
            "bash",
            "Output is being written to: /tmp/tasks/bq7x.output.txt".to_string(),
        ),
        ("bash", "see /tmp/tasks/bq7x.output".to_string()),
        (
            "bash",
            "Output is being written to: tmp/tasks/bq7x.output.".to_string(),
        ),
        (
            "bash",
            "Output: /tmp\n/claude/tasks/bq7x.output".to_string(),
        ),
        ("bash", "Output: /tmp/tasks/xbq7x.output".to_string()),
    ];
    for (tool_use_id, text) in cases {
        let mut runtime = AsyncTaskRuntime::new(true);
        runtime.tool_results(
            &json!([{"type": "tool_result", "tool_use_id": tool_use_id, "content": text}]),
        );
        let published = runtime.task_started(&started(true));
        assert!(
            published[0].get("outputFilePath").is_none(),
            "{text}: {published:?}"
        );
    }
}

/// T-95 — two paths on one line are not glued into one.
#[test]
fn t3a_two_paths_on_one_line_are_not_glued() {
    let line = |second: &str| result_block(&format!("files: /a/tasks/b0.output {second}"));
    let b1 = json!({"task_id": "b1", "task_type": "local_bash", "description": "npm test",
        "tool_use_id": "bash", "is_backgrounded": true});
    let mut glued = AsyncTaskRuntime::new(true);
    glued.tool_results(&line("/b/tasks/b1.output"));
    assert!(glued.task_started(&b1)[0].get("outputFilePath").is_none());
    let mut separated = AsyncTaskRuntime::new(true);
    separated.tool_results(&line("and output: /b/tasks/b1.output."));
    assert_eq!(
        separated.task_started(&b1)[0]["outputFilePath"],
        "/b/tasks/b1.output"
    );
}

/// T-95 — a text of two million characters without a usable boundary is read in
/// linear time.
#[test]
fn t3a_a_long_text_is_read_in_linear_time() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let text = "/private/tmp/claude/project/session/tasks/bX.output\n".repeat(42_000);
    assert!(text.len() > 2_000_000);
    let begun = std::time::Instant::now();
    runtime.tool_results(&result_block(&text));
    runtime.tool_results(&result_block(&text.replace('\n', " ")));
    assert!(
        begun.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
    let published = runtime.task_started(
        &json!({"task_id": "bX", "task_type": "local_bash", "description": "t", "tool_use_id": "bash", "is_backgrounded": true}),
    );
    assert!(published[0].get("outputFilePath").is_none());
}

/// T-95 — a structured output path wins over the tool result's.
#[test]
fn t3a_structured_output_path_wins_over_the_tool_result() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let path = "/private/tmp/claude-501/project/session/tasks/bq7x.output";
    runtime.tool_results(&result_block(
        &background_texts("bq7x", "/tmp/tasks/bq7x.output")[1],
    ));
    let mut message = started(true);
    message["output_file"] = json!(path);
    let published = runtime.task_started(&message);
    assert_eq!(published[0]["outputFilePath"], path);
}

/// N-69 — a terminal edge for a task nobody announced is kept; the Bash result
/// that proves the backgrounding then announces it and its end, once.
#[test]
fn t3a_terminal_tombstone_waits_for_a_late_bash_result() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let first = runtime.task_notification(&json!({"task_id": "fast-shell", "status": "completed",
        "summary": "Already done", "output_file": "/tmp/tasks/fast-shell.output"}));
    assert!(first.is_empty());
    let mut published = runtime.task_backgrounded(&json!({"taskId": "fast-shell",
        "taskType": "local_bash", "description": "Fast build", "isBackgrounded": true,
        "toolCallId": "bash-tool"}));
    published.extend(runtime.task_updated("fast-shell", &json!({"status": "running"})));
    assert_eq!(
        kinds(&published),
        ["async_task_spawned", "async_task_state_update"]
    );
    assert_eq!(published[0]["description"], "Fast build");
    assert_eq!(
        published[0]["outputFilePath"],
        "/tmp/tasks/fast-shell.output"
    );
    // The spawn carried the output path and the tool call: the state does not
    // repeat them.
    assert_eq!(
        published[1],
        json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": "fast-shell",
            "state": "completed", "summary": "Already done"})
    );
}

/// N-69 — the same for a terminal `task_updated`.
#[test]
fn t3a_terminal_task_updated_tombstone_waits_for_promotion() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let first = runtime.task_updated("fast-shell", &json!({"status": "failed", "error": "boom"}));
    assert!(first.is_empty());
    let published = runtime.task_backgrounded(&json!({"task_id": "fast-shell",
        "task_type": "local_bash", "description": "Fast build", "is_backgrounded": true}));
    assert_eq!(
        kinds(&published),
        ["async_task_spawned", "async_task_state_update"]
    );
    assert_eq!(published[1]["state"], "failed");
    assert_eq!(published[1]["summary"], "boom");
}

/// N-62 — promotion ignores a subagent task and a monitor task.
#[test]
fn t3a_backgrounding_ignores_subagent_and_monitor_tasks() {
    let mut runtime = AsyncTaskRuntime::new(true);
    assert!(runtime
        .task_backgrounded(
            &json!({"taskId": "agent", "taskType": "local_agent", "toolCallId": "t"})
        )
        .is_empty());
    assert!(runtime
        .task_backgrounded(
            &json!({"taskId": "mon", "taskType": "local_monitor", "toolCallId": "t"})
        )
        .is_empty());
    assert!(runtime
        .task_notification(&json!({"taskId": "agent", "status": "completed"}))
        .is_empty());
}

/// N-64 — a held task's state change waits for its spawn and follows it in order,
/// with the progress beat that brought the tool call id.
#[test]
fn t3a_held_state_change_follows_the_spawn_in_order() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let mut published = runtime
        .task_started(&json!({"task_id": "w1", "task_type": "local_workflow",
        "description": "Deploy", "workflow_name": "Deploy"}));
    published.extend(runtime.task_updated("w1", &json!({"status": "paused"})));
    assert!(published.is_empty(), "held");
    published.extend(
        runtime.task_progress(&json!({"task_id": "w1", "description": "Deploying",
        "summary": "step 1", "tool_use_id": "toolu_WF"})),
    );
    assert_eq!(
        kinds(&published),
        [
            "async_task_spawned",
            "async_task_state_update",
            "async_task_progress"
        ]
    );
    assert_eq!(published[1]["state"], "paused");
    assert_eq!(published[2]["summary"], "step 1");
}

/// B-108 — the tasks held at the prompt result are announced in the order they
/// were first seen, not in a hash order.
#[test]
fn t3a_release_held_announces_in_first_seen_order() {
    let mut runtime = AsyncTaskRuntime::new(true);
    let ids = ["zz", "aa", "mm", "bb", "yy", "cc", "xx", "dd"];
    for id in ids {
        runtime.task_started(
            &json!({"task_id": id, "task_type": "local_workflow", "description": id}),
        );
    }
    let published = runtime.release_held();
    let announced: Vec<&str> = published
        .iter()
        .map(|update| update["asyncTaskId"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(announced, ids);
}

/// T-97 / T-96 — which Bash calls the client gets marked as backgrounded: the
/// structured task's call, a call a known task names, or a `run_in_background`
/// call that did not fail; nothing for a runtime without the lifecycle or a
/// foreground call.
#[test]
fn t3a_backgrounded_bash_calls() {
    let tools = |id: &str| match id {
        "structured" | "known" | "input-ok" | "input-failed" | "plain" => Some(json!({
            "name": "Bash",
            "input": {"command": "x", "run_in_background": id.starts_with("input")},
        })),
        "read" => Some(json!({"name": "Read", "input": {"run_in_background": true}})),
        _ => None,
    };
    let content = json!([
        {"type": "tool_result", "tool_use_id": "structured", "content": ""},
        {"type": "tool_result", "tool_use_id": "known", "content": ""},
        {"type": "tool_result", "tool_use_id": "input-ok", "content": ""},
        {"type": "tool_result", "tool_use_id": "input-failed", "content": "", "is_error": true},
        {"type": "tool_result", "tool_use_id": "plain", "content": ""},
        {"type": "tool_result", "tool_use_id": "read", "content": ""},
    ]);
    let mut runtime = AsyncTaskRuntime::new(true);
    runtime.task_started(
        &json!({"task_id": "k1", "task_type": "local_bash", "description": "k",
        "tool_use_id": "known", "is_backgrounded": true}),
    );
    let ids = backgrounded_bash_tool_call_ids(&content, &tools, &runtime, Some("structured"));
    let mut ids: Vec<String> = ids.into_iter().collect();
    ids.sort();
    assert_eq!(ids, ["input-ok", "known", "structured"]);
    let off = AsyncTaskRuntime::new(false);
    assert!(backgrounded_bash_tool_call_ids(&content, &tools, &off, Some("structured")).is_empty());
}

/// N-61 — a task a Monitor tool call started is never announced, whether the call
/// id is known when the task starts, when a held task gets its id, or when it ends.
#[test]
fn t3a_monitor_tool_tasks_are_never_announced() {
    let mut runtime = AsyncTaskRuntime::new(true);
    runtime.hide_tool_call("mon-call");
    let mut published = runtime
        .task_started(&json!({"task_id": "m1", "task_type": "local_workflow",
        "description": "watch", "tool_use_id": "mon-call"}));
    published.extend(runtime.task_notification(&json!({"task_id": "m1", "status": "completed"})));
    // Held first, then the progress beat brings the Monitor call id.
    published.extend(runtime.task_started(
        &json!({"task_id": "m2", "task_type": "local_workflow", "description": "watch"}),
    ));
    published.extend(runtime.task_progress(&json!({"task_id": "m2", "tool_use_id": "mon-call"})));
    published.extend(runtime.task_notification(&json!({"task_id": "m2", "status": "completed"})));
    // Held, then it ends with the Monitor call id on the terminal notification.
    published.extend(runtime.task_started(
        &json!({"task_id": "m3", "task_type": "local_workflow", "description": "watch"}),
    ));
    published.extend(runtime.task_notification(
        &json!({"task_id": "m3", "status": "completed", "tool_use_id": "mon-call"}),
    ));
    published.extend(runtime.release_held());
    assert!(published.is_empty(), "{published:?}");
    // A task of another tool is still announced.
    let published = runtime.task_started(&json!({"task_id": "o1", "task_type": "local_workflow",
        "description": "other", "tool_use_id": "other-call"}));
    assert_eq!(kinds(&published), ["async_task_spawned"]);
}
