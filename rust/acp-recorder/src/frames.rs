//! Wire frame types recorded by the recorder.

use serde::Serialize;
use serde_json::Value;

/// The direction a frame travelled across the client/agent boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// client -> agent (written to the agent's stdin).
    Send,
    /// agent -> client (read from the agent's stdout).
    Recv,
}

/// One ordered JSON-RPC frame plus its direction.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub direction: Direction,
    pub json: Value,
}

impl Frame {
    /// A frame sent client -> agent.
    pub fn send(json: Value) -> Self {
        Self {
            direction: Direction::Send,
            json,
        }
    }

    /// A frame received agent -> client.
    pub fn recv(json: Value) -> Self {
        Self {
            direction: Direction::Recv,
            json,
        }
    }
}

/// A single JSONL output line for a frame.
#[derive(Debug, Clone, Serialize)]
pub struct FrameLine {
    pub direction: Direction,
    pub frame: Value,
}

impl From<&Frame> for FrameLine {
    fn from(frame: &Frame) -> Self {
        Self {
            direction: frame.direction,
            frame: frame.json.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_line_preserves_direction_and_json() {
        let frame = Frame::send(serde_json::json!({"method": "initialize"}));
        let line = FrameLine::from(&frame);
        assert_eq!(line.direction, Direction::Send);
        assert_eq!(line.frame, serde_json::json!({"method": "initialize"}));
    }

    #[test]
    fn direction_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Direction::Send).unwrap(),
            r#""send""#
        );
        assert_eq!(
            serde_json::to_string(&Direction::Recv).unwrap(),
            r#""recv""#
        );
    }
}
