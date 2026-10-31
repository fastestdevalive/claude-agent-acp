//! Line codec for the child's stdout (Decision D13 / item 3.3).
//!
//! `claude` speaks newline-delimited JSON on stdout. This module:
//!
//! - splits an arbitrary byte stream into complete lines (partial-line buffer,
//!   UTF-8 carry across read boundaries — INV-2, INV-3),
//! - parses each complete line as JSON, **skipping** lines that are not valid
//!   JSON rather than failing the stream (INV-1 / R6),
//! - runs in its **own** task and feeds an unbounded `mpsc` of parsed values;
//!   nothing else reads the pipe (INV-32).
//!
//! The [`LineCodec`] byte-splitting is a pure, unit-testable struct; the async
//! reader task wraps it. `select!`-ing on the returned `mpsc::UnboundedReceiver`
//! is cancel-safe: a cancel that races the reader task never drops a line that
//! was already enqueued (INV-32).

use std::io;

use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// A pure newline splitter that also handles CRLF and UTF-8 multi-byte
/// characters split across read boundaries.
///
/// Bytes are accumulated until a `\n` is seen; only then is a complete line
/// emitted. Because a line is only decoded once its terminating newline has
/// arrived, a UTF-8 character that happens to straddle two `push` calls is
/// fully reassembled inside the buffer before decoding (INV-3).
#[derive(Debug, Default)]
pub struct LineCodec {
    pending: Vec<u8>,
}

impl LineCodec {
    /// A fresh codec with no buffered bytes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are any buffered bytes not yet emitted as a line.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Feed a chunk of bytes, appending every complete line (newline-delimited)
    /// to `out`. A trailing partial line stays buffered for the next call.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Vec<u8>>) {
        self.pending.extend_from_slice(bytes);
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop(); // drop the trailing '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // CRLF: drop the '\r' left before '\n'
            }
            out.push(line);
        }
    }

    /// Flush any remaining partial line at end-of-stream (no trailing newline).
    pub fn finish(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.pending.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.pending);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        out.push(line);
    }
}

/// Parse one raw line into JSON. A line that is not valid JSON yields `None`
/// and is skipped (logged by the caller) rather than terminating the stream
/// (INV-1, `sdk.mjs` `readline`-based non-fatal parsing).
pub fn parse_line(line: &[u8]) -> Option<Value> {
    serde_json::from_slice(line).ok()
}

/// Errors produced by the codec reader task.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("failed to read child stdout: {0}")]
    Read(#[from] io::Error),
}

/// Spawn the line-codec reader as its own task over `reader`.
///
/// The task reads to EOF, splits lines, parses each as JSON, and sends every
/// parsed value into an unbounded `mpsc`. Garbage lines are dropped (INV-1).
/// The returned receiver is the only way to observe the stream — nothing else
/// touches the pipe (INV-32). The task handle lets the owner await reader EOF.
pub fn spawn_codec(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
) -> (
    mpsc::UnboundedReceiver<Value>,
    tokio::task::JoinHandle<Result<(), CodecError>>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let handle = tokio::spawn(async move {
        let mut codec = LineCodec::new();
        let mut buf = [0u8; 16 * 1024];
        let mut lines: Vec<Vec<u8>> = Vec::new();
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            lines.clear();
            codec.push(&buf[..n], &mut lines);
            for line in lines.drain(..) {
                if let Some(value) = parse_line(&line) {
                    let _ = tx.send(value);
                }
            }
        }
        lines.clear();
        codec.finish(&mut lines);
        for line in lines.drain(..) {
            if let Some(value) = parse_line(&line) {
                let _ = tx.send(value);
            }
        }
        Ok(())
    });
    (rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// 3.T2 — a garbage (non-JSON) line is skipped; the next valid line parses.
    #[test]
    fn inv_01_garbage_line_skipped() {
        let mut codec = LineCodec::new();
        let mut out = Vec::new();
        codec.push(b"this is not json\n{\"type\":\"result\"}\n", &mut out);
        assert_eq!(out.len(), 2);
        // Garbage line -> skipped.
        assert!(parse_line(&out[0]).is_none());
        // Next valid line parses.
        let value = parse_line(&out[1]).expect("valid line parses");
        assert_eq!(value["type"], "result");
    }

    /// 3.T3 — a 10 MB single line parses; a message split across 3 reads
    /// reassembles into one line.
    #[test]
    fn inv_02_partial_and_huge_lines() {
        // A message split across 3 reads reassembles.
        let mut codec = LineCodec::new();
        let mut out = Vec::new();
        codec.push(b"{\"a\":", &mut out);
        codec.push(b"[1,2,", &mut out);
        codec.push(b"3]}\n", &mut out);
        assert_eq!(out.len(), 1);
        let value = parse_line(&out[0]).expect("reassembled line parses");
        assert_eq!(value["a"], serde_json::json!([1, 2, 3]));

        // A 10 MB single line parses.
        let payload = "x".repeat(10 * 1024 * 1024);
        let mut big = Vec::new();
        big.extend_from_slice(b"{\"type\":\"user\",\"big\":\"");
        big.extend_from_slice(payload.as_bytes());
        big.extend_from_slice(b"\"}\n");
        let mut codec2 = LineCodec::new();
        let mut out2 = Vec::new();
        codec2.push(&big, &mut out2);
        assert_eq!(out2.len(), 1);
        let value = parse_line(&out2[0]).expect("10 MB line parses");
        assert_eq!(value["big"].as_str().map(str::len), Some(payload.len()));
    }

    /// 3.T4 — a 4-byte UTF-8 character split across reads decodes intact.
    #[test]
    fn inv_03_utf8_split() {
        // U+1F600 GRINNING FACE is a 4-byte UTF-8 sequence: f0 9f 98 80.
        let bytes = "{\"emoji\":\"\u{1F600}\"}".as_bytes();
        assert_eq!(bytes[9..13].len(), 4);

        let mut codec = LineCodec::new();
        let mut out = Vec::new();
        // Split the 4-byte char across 3 reads (and split mid-line generally).
        codec.push(&bytes[..9], &mut out); // up to but not incl. first emoji byte
        codec.push(&bytes[9..11], &mut out); // f0 9f
        codec.push(&bytes[11..], &mut out); // 98 80 } and newline
        codec.push(b"\n", &mut out);
        assert_eq!(out.len(), 1);
        let value = parse_line(&out[0]).expect("utf8 line parses");
        assert_eq!(value["emoji"], "\u{1F600}");
    }

    /// 3.T9 — cancelling the consumer's `select!` mid-stream loses no line.
    ///
    /// The reader task enqueues many lines; the consumer races `recv()` against
    /// a cancel token and the cancel fires while lines are still buffered.
    /// Every line the producer already sent is still delivered in order
    /// afterwards (INV-32) — a cancel never drops a queued line.
    #[tokio::test]
    async fn inv_32_codec_cancel_safe() {
        let total = 200;
        let mut input = Vec::new();
        for i in 0..total {
            input.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
        }
        let reader = std::io::Cursor::new(input);
        let (mut rx, handle) = spawn_codec(reader);

        let mut seen: Vec<i64> = Vec::new();
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();

        // Read the first line, then arm the cancel so the select! hits the
        // cancel branch while the channel still holds many pending lines.
        let v = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("line available")
            .expect("stream open");
        seen.push(v["i"].as_i64().expect("i is an int"));
        let _ = cancel_tx.send(());

        // The next read races recv() against the (now-fired) cancel token; the
        // cancel branch is taken (returns None), but the line is NOT dropped —
        // a subsequent plain read still delivers it (INV-32).
        let cancel_result = recv_select(&mut rx, &mut cancel_rx).await;
        assert!(cancel_result.is_none(), "select! took the cancel branch");
        let next = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("line available")
            .expect("stream open");
        seen.push(next["i"].as_i64().expect("i is an int"));

        // Drain the rest; every line arrives, in order, none lost.
        while let Some(v) = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no hang while draining")
        {
            seen.push(v["i"].as_i64().expect("i is an int"));
        }
        assert_eq!(seen.len(), total, "every line must arrive");
        assert!(
            seen.windows(2).all(|w| w[0] + 1 == w[1]),
            "lines must arrive in order"
        );

        let _ = timeout(Duration::from_secs(5), handle)
            .await
            .expect("codec task finishes")
            .expect("codec task ok");
    }

    /// Await `recv()` via a `select!` that can take a cancel branch.
    async fn recv_select(
        rx: &mut mpsc::UnboundedReceiver<Value>,
        cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> Option<Value> {
        tokio::select! {
            biased;
            _ = cancel_rx => None,
            line = rx.recv() => line,
        }
    }

    use std::time::Duration;
}
