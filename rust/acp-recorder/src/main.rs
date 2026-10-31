//! `acp-recorder` binary: `acp-recorder <agent-cmd> <script.json> <out.jsonl>`.
//!
//! Runs the agent under test, drives `script.json`, and writes the ordered
//! frames to `out.jsonl` (one JSON object per line).

use std::process::ExitCode;

use acp_recorder::frames::Frame;
use acp_recorder::Script;

/// Parse the three positional CLI arguments. Returns `None` on a usage error.
fn parse_args(args: &[String]) -> Option<(&str, &str, &str)> {
    if args.len() != 3 {
        return None;
    }
    Some((&args[0], &args[1], &args[2]))
}

/// Render frames to JSONL text (one `FrameLine` object per line).
fn render_frames(frames: &[Frame]) -> String {
    let mut output = String::new();
    for frame in frames {
        let line = acp_recorder::frames::FrameLine::from(frame);
        if let Ok(json) = serde_json::to_string(&line) {
            output.push_str(&json);
            output.push('\n');
        }
    }
    output
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((agent_cmd, script_path, out_path)) = parse_args(&args) else {
        eprintln!("usage: acp-recorder <agent-cmd> <script.json> <out.jsonl>");
        return ExitCode::from(2);
    };

    let script_text = match std::fs::read_to_string(script_path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("failed to read script `{script_path}`: {error}");
            return ExitCode::FAILURE;
        }
    };

    let script = match Script::parse(&script_text) {
        Ok(script) => script,
        Err(error) => {
            eprintln!("failed to parse script `{script_path}`: {error}");
            return ExitCode::FAILURE;
        }
    };

    let frames = match acp_recorder::record(agent_cmd, &script).await {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("recording failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    match std::fs::write(out_path, render_frames(&frames)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("failed to write output `{out_path}`: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_arguments() {
        let args = vec![
            "echo-agent".to_string(),
            "script.json".to_string(),
            "out.jsonl".to_string(),
        ];
        assert_eq!(
            parse_args(&args),
            Some(("echo-agent", "script.json", "out.jsonl"))
        );
    }

    #[test]
    fn rejects_wrong_argument_count() {
        assert_eq!(parse_args(&[]), None);
        assert_eq!(parse_args(&["a".to_string()]), None);
        assert_eq!(
            parse_args(&[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]),
            None
        );
    }

    #[test]
    fn renders_frames_as_jsonl() {
        let frames = vec![
            Frame::send(serde_json::json!({"method": "initialize", "id": 1})),
            Frame::recv(serde_json::json!({"id": 1, "result": {"protocolVersion": 1}})),
        ];
        let text = render_frames(&frames);
        let mut lines = text.lines();
        let first: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            first,
            serde_json::json!({"direction": "send", "frame": {"method": "initialize", "id": 1}})
        );
        let second: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            second,
            serde_json::json!({"direction": "recv", "frame": {"id": 1, "result": {"protocolVersion": 1}}})
        );
        assert!(lines.next().is_none());
    }
}
