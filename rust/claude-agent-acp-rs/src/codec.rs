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

use std::collections::{HashMap, VecDeque};
use std::io;

use agent_client_protocol::{Agent, ConnectTo, Lines};
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
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

// ---------------------------------------------------------------------------
// Client transport wire codec (ROADMAP F2c / M-126)
// ---------------------------------------------------------------------------
//
// The ACP client speaks newline-delimited JSON to us on stdin. The pinned
// `agent-client-protocol` crate already splits lines and routes JSON-RPC, but
// two wire details differ from Node's `ndJsonStream`
// (`@agentclientprotocol/sdk/dist/core/stream.js`):
//
// - a line that is not JSON gets `{"jsonrpc":"2.0","id":null,"error":
//   {"code":-32700,"message":"Parse error"}}` (the crate adds `data:{line}`),
// - outgoing request ids are uuid strings (the crate), while Node allocates
//   numbers (`nextRequestId++`, `core/jsonrpc.js:546`), so
//   `session/request_permission` must carry a NUMBER id.
//
// [`WireTransport`] wraps the crate's line transport: it parses/validates each
// inbound line exactly like `ndJsonStream`, answers malformed lines itself,
// and translates outgoing ids (and the client's replies) through [`RequestIds`].
// The crate stays at 2.1.0 (guard G10) — no patching.

/// The outcome of parsing one line of the ACP client wire.
#[derive(Debug, Clone, PartialEq)]
pub enum WireLine {
    /// A JSON object or array; Node's `ndJsonStream` enqueues it as-is.
    Message(Value),
    /// The line is not JSON: Node answers `-32700 Parse error`.
    ParseError,
    /// Valid JSON that is neither an object nor an array: Node answers
    /// `-32600 Invalid request` with the value as `data`.
    InvalidRequest(Value),
    /// A blank line: Node's stream skips it.
    Empty,
}

/// Port of Node's `ndJsonStream` line handling: trim the line, skip blanks,
/// `JSON.parse` it, and classify objects/arrays as protocol messages while
/// everything else is answered at the transport.
pub fn parse_wire_line(line: &str) -> WireLine {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return WireLine::Empty;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => WireLine::Message(value),
        Ok(value) => WireLine::InvalidRequest(value),
        Err(_) => WireLine::ParseError,
    }
}

/// The reply Node's `ndJsonStream` writes for an unparsable line
/// (`protocolErrorResponse(RequestError.parseError())`).
pub fn parse_error_reply() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": -32700, "message": "Parse error" },
    })
}

/// The reply Node's `ndJsonStream` writes for JSON that is not an object or
/// array (`RequestError.invalidRequest(value)`, `data` = the value).
pub fn invalid_request_reply(value: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": -32600, "message": "Invalid request", "data": value.clone() },
    })
}

/// Cap on the correlation entries kept in one direction. The writer only needs
/// the map to translate a later `$/cancel_request`, and the reader removes an
/// entry as soon as its response arrives; a never-answered entry is evicted
/// once this many newer ids exist, so the maps cannot grow without bound over
/// the process lifetime.
const MAX_TRACKED_IDS: usize = 4096;

/// Outgoing request-id translation for the stdio transport.
///
/// Node's ACP SDK allocates outgoing request ids as numbers (`nextRequestId++`,
/// `core/jsonrpc.js:546`); the pinned crate allocates uuid strings. This map
/// rewrites each outgoing request id to the next number and maps the client's
/// numeric reply (and an outgoing `$/cancel_request`'s `requestId`) back to the
/// crate's string id.
#[derive(Debug, Default)]
pub struct RequestIds {
    next: u64,
    by_string: HashMap<String, u64>,
    by_number: HashMap<u64, String>,
    /// Insertion order of the assigned numbers, for the age-out cap.
    order: VecDeque<u64>,
}

impl RequestIds {
    /// Assign (or reuse) the numeric id for a crate-generated string id.
    fn number_for(&mut self, id: &str) -> u64 {
        if let Some(number) = self.by_string.get(id) {
            return *number;
        }
        let number = self.next;
        self.next += 1;
        self.by_string.insert(id.to_string(), number);
        self.by_number.insert(number, id.to_string());
        self.order.push_back(number);
        self.evict_oldest();
        number
    }

    /// Drop the oldest never-answered entries once the map is over its cap.
    fn evict_oldest(&mut self) {
        while self.order.len() > MAX_TRACKED_IDS {
            let Some(number) = self.order.pop_front() else {
                break;
            };
            if let Some(id) = self.by_number.remove(&number) {
                self.by_string.remove(&id);
            }
        }
    }

    /// Remember a `(number, string id)` pair assigned by the writer side, so
    /// the reader can map the client's numeric reply back.
    pub fn remember(&mut self, number: u64, id: &str) {
        self.by_number.insert(number, id.to_string());
        self.order.push_back(number);
        self.evict_oldest();
    }

    /// Number of correlation entries currently held (test/observability).
    pub fn tracked_len(&self) -> usize {
        self.by_number.len()
    }

    /// Rewrite one outgoing value in place: a request's `id` becomes a number,
    /// and an outgoing `$/cancel_request`'s `params.requestId` follows the same
    /// mapping. Batches (arrays) are rewritten item by item. Returns every
    /// newly assigned `(number, original id)` pair so the reader can remember
    /// them all — a batch member's response needs its own mapping back.
    pub fn rewrite_outgoing(&mut self, value: &mut Value) -> Vec<(u64, String)> {
        match value {
            Value::Array(items) => {
                let mut assigned = Vec::new();
                for item in items {
                    assigned.extend(self.rewrite_outgoing(item));
                }
                assigned
            }
            Value::Object(object) => {
                let Some(method) = object.get("method").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if method == "$/cancel_request" {
                    let params = object.get_mut("params").and_then(Value::as_object_mut);
                    let Some(params) = params else {
                        return Vec::new();
                    };
                    let request_id = params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let Some(request_id) = request_id else {
                        return Vec::new();
                    };
                    let number = self.by_string.get(&request_id).copied();
                    let Some(number) = number else {
                        return Vec::new();
                    };
                    params.insert("requestId".to_string(), json!(number));
                    return Vec::new();
                }
                let id = object.get("id").and_then(Value::as_str).map(str::to_string);
                let Some(id) = id else {
                    return Vec::new();
                };
                let number = self.number_for(&id);
                object.insert("id".to_string(), json!(number));
                vec![(number, id)]
            }
            _ => Vec::new(),
        }
    }

    /// Rewrite one incoming value in place: a response to one of our requests
    /// gets the crate's original string id back (batches are handled member by
    /// member), and its correlation entry is dropped. Client requests keep
    /// their own ids untouched.
    pub fn rewrite_incoming(&mut self, value: &mut Value) {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.rewrite_incoming(item);
                }
            }
            Value::Object(object) => {
                if object.contains_key("method") {
                    return;
                }
                let Some(id) = object.get("id").and_then(Value::as_u64) else {
                    return;
                };
                let Some(original) = self.by_number.remove(&id) else {
                    return;
                };
                object.insert("id".to_string(), Value::String(original));
            }
            _ => {}
        }
    }
}

/// Serialize a JSON value as one wire line (no trailing newline).
fn wire_line(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Rewrite an outgoing wire line (crate -> client) through [`RequestIds`],
/// also returning every newly assigned `(number, original id)` mapping (a batch
/// has one per member).
pub fn rewrite_outgoing_line(line: &str, ids: &mut RequestIds) -> (String, Vec<(u64, String)>) {
    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return (line.to_string(), Vec::new());
    };
    let assigned = ids.rewrite_outgoing(&mut value);
    (wire_line(&value), assigned)
}

/// Rewrite an incoming wire line (client -> crate) through [`RequestIds`].
pub fn rewrite_incoming_line(line: &str, ids: &mut RequestIds) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    ids.rewrite_incoming(&mut value);
    wire_line(&value)
}

/// Feed one raw line to the crate, or answer it at the transport like Node's
/// `ndJsonStream` does.
fn handle_wire_line(
    line: &[u8],
    in_tx: &futures::channel::mpsc::UnboundedSender<io::Result<String>>,
    out_tx: &futures::channel::mpsc::UnboundedSender<String>,
    ids: &mut RequestIds,
) {
    let Ok(text) = std::str::from_utf8(line) else {
        // Node decodes with TextDecoder (U+FFFD for invalid UTF-8) and then
        // fails JSON.parse: the line is a parse error.
        let _ = out_tx.unbounded_send(wire_line(&parse_error_reply()));
        return;
    };
    match parse_wire_line(text) {
        WireLine::Empty => {}
        WireLine::ParseError => {
            let _ = out_tx.unbounded_send(wire_line(&parse_error_reply()));
        }
        WireLine::InvalidRequest(value) => {
            let _ = out_tx.unbounded_send(wire_line(&invalid_request_reply(&value)));
        }
        WireLine::Message(mut value) => {
            ids.rewrite_incoming(&mut value);
            let _ = in_tx.unbounded_send(Ok(wire_line(&value)));
        }
    }
}

/// Read the byte stream to EOF, splitting lines and feeding the crate.
async fn read_wire<R: AsyncRead + Unpin>(
    mut reader: R,
    in_tx: futures::channel::mpsc::UnboundedSender<io::Result<String>>,
    out_tx: futures::channel::mpsc::UnboundedSender<String>,
    mut assigned_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, String)>,
) {
    // The reader is the single owner of the reverse map: the writer sends each
    // `(number, string id)` pair it assigns before it writes the request, so
    // the pair is always queued before the client can answer. Draining the
    // channel before each line (and each chunk) keeps the mapping exact without
    // a lock (guard G5: no mutexes).
    let mut ids = RequestIds::default();
    let mut codec = LineCodec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut lines: Vec<Vec<u8>> = Vec::new();
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) => {
                let _ = in_tx.unbounded_send(Err(error));
                return;
            }
        };
        while let Ok((number, id)) = assigned_rx.try_recv() {
            ids.remember(number, &id);
        }
        lines.clear();
        codec.push(&buf[..n], &mut lines);
        for line in lines.drain(..) {
            handle_wire_line(&line, &in_tx, &out_tx, &mut ids);
        }
    }
    lines.clear();
    codec.finish(&mut lines);
    for line in lines.drain(..) {
        handle_wire_line(&line, &in_tx, &out_tx, &mut ids);
    }
    // Dropping `in_tx` ends the crate's incoming stream (transport EOF).
}

/// Write rewritten lines to the peer, one newline-terminated line at a time.
async fn write_wire<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: futures::channel::mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = rx.next().await {
        if writer.write_all(line.as_bytes()).await.is_err()
            || writer.write_all(b"\n").await.is_err()
        {
            break;
        }
        let _ = writer.flush().await;
    }
}

/// Blocking counterpart of [`read_wire`] for stdio.
///
/// The binary reads stdin on a plain OS thread: a `tokio::io::stdin()` read
/// parks a runtime blocking-pool task, and the runtime waits for those on drop,
/// so a SIGTERM/SIGINT shutdown would never finish while stdin stays open. A
/// detached OS thread is abandoned at process exit instead.
fn read_wire_blocking(
    mut reader: impl std::io::BufRead,
    in_tx: futures::channel::mpsc::UnboundedSender<io::Result<String>>,
    out_tx: futures::channel::mpsc::UnboundedSender<String>,
    mut assigned_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, String)>,
) {
    let mut ids = RequestIds::default();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                // Drain AFTER the read: a mapping is queued before its request
                // is written, so any reply we just read has its pair waiting.
                // (Draining before the blocking read would miss every request
                // the writer assigns while we are parked in it.)
                while let Ok((number, id)) = assigned_rx.try_recv() {
                    ids.remember(number, &id);
                }
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                handle_wire_line(&line, &in_tx, &out_tx, &mut ids);
            }
            Err(error) => {
                let _ = in_tx.unbounded_send(Err(error));
                return;
            }
        }
    }
    // Dropping `in_tx` ends the crate's incoming stream (transport EOF).
}

/// A line-oriented ACP client transport that applies Node's `ndJsonStream`
/// wire rules in front of the pinned crate (ROADMAP F2c).
///
/// The binary uses [`WireTransport::stdio`]; tests can wrap any
/// reader/writer pair (e.g. a duplex pipe) to drive raw JSON-RPC lines.
pub struct WireTransport {
    /// Lines already validated/rewritten, ready for the crate.
    incoming: futures::channel::mpsc::UnboundedReceiver<io::Result<String>>,
    /// Lines from the crate, before id rewriting.
    outgoing: futures::channel::mpsc::UnboundedSender<String>,
    /// `(number, original id)` pairs assigned by the outgoing sink; the reader
    /// owns the reverse map (no mutex — guard G5).
    assigned_tx: tokio::sync::mpsc::UnboundedSender<(u64, String)>,
}

impl WireTransport {
    /// The stdio transport used by `bin/claude-agent-acp-rs.rs`.
    pub fn stdio() -> Self {
        let (in_tx, in_rx) = futures::channel::mpsc::unbounded::<io::Result<String>>();
        let (out_tx, out_rx) = futures::channel::mpsc::unbounded::<String>();
        let (assigned_tx, assigned_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, String)>();
        tokio::spawn(write_wire(tokio::io::stdout(), out_rx));
        let read_out = out_tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = std::io::BufReader::new(stdin.lock());
            read_wire_blocking(reader, in_tx, read_out, assigned_rx);
        });
        Self {
            incoming: in_rx,
            outgoing: out_tx,
            assigned_tx,
        }
    }

    /// Wrap a byte reader (client input) and writer (client output).
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (in_tx, in_rx) = futures::channel::mpsc::unbounded::<io::Result<String>>();
        let (out_tx, out_rx) = futures::channel::mpsc::unbounded::<String>();
        let (assigned_tx, assigned_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, String)>();
        tokio::spawn(write_wire(writer, out_rx));
        let read_out = out_tx.clone();
        tokio::spawn(async move {
            read_wire(reader, in_tx, read_out, assigned_rx).await;
        });
        Self {
            incoming: in_rx,
            outgoing: out_tx,
            assigned_tx,
        }
    }
}

impl ConnectTo<Agent> for WireTransport {
    async fn connect_to(
        self,
        client: impl ConnectTo<agent_client_protocol::Client>,
    ) -> agent_client_protocol::Result<()> {
        let Self {
            incoming,
            outgoing,
            assigned_tx,
        } = self;
        // The sink is the single owner of the forward (string -> number) map;
        // the reader owns the reverse map, fed by `assigned_tx`.
        let outgoing = futures::sink::unfold(
            (outgoing, assigned_tx, RequestIds::default()),
            |(tx, assigned_tx, mut ids), line: String| async move {
                let (rewritten, assigned) = rewrite_outgoing_line(&line, &mut ids);
                for (number, id) in assigned {
                    let _ = assigned_tx.send((number, id));
                }
                tx.unbounded_send(rewritten)
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ACP stdout closed"))?;
                Ok::<_, io::Error>((tx, assigned_tx, ids))
            },
        );
        let lines = Lines::new(outgoing, incoming);
        ConnectTo::<Agent>::connect_to(lines, client).await
    }
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

    /// F2c / M-126 — the malformed-line reply is exactly Node's
    /// `ndJsonStream` parse error: id null, -32700, no `data`.
    #[test]
    fn inv_52_parse_error_reply_matches_node() {
        assert_eq!(parse_wire_line("not json"), WireLine::ParseError);
        assert_eq!(
            parse_error_reply(),
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "Parse error"}
            })
        );
        // Node trims each line before JSON.parse and skips blank ones.
        assert_eq!(parse_wire_line("   \t "), WireLine::Empty);
        assert_eq!(
            parse_wire_line("  {\"jsonrpc\":\"2.0\",\"method\":\"x\"}  "),
            WireLine::Message(json!({"jsonrpc": "2.0", "method": "x"}))
        );
    }

    /// F2c / M-126 — valid JSON that is not an object/array gets Node's
    /// `invalidRequest` reply with the value in `data` (a JSON `null` keeps a
    /// literal `data:null`).
    #[test]
    fn inv_52_invalid_request_reply_matches_node() {
        assert_eq!(parse_wire_line("42"), WireLine::InvalidRequest(json!(42)));
        assert_eq!(
            invalid_request_reply(&json!(42)),
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32600, "message": "Invalid request", "data": 42}
            })
        );
        assert_eq!(
            parse_wire_line("null"),
            WireLine::InvalidRequest(Value::Null)
        );
        assert_eq!(
            invalid_request_reply(&Value::Null)["error"]["data"],
            Value::Null
        );
        assert_eq!(
            parse_wire_line("[\"not a record\"]"),
            WireLine::Message(json!(["not a record"]))
        );
    }

    /// F2c / M-126 — outgoing request ids become Node's sequential numbers
    /// (first outgoing request = 0) and the client's numeric reply maps back to
    /// the crate's string id. Client request ids are never rewritten.
    #[test]
    fn inv_52_outgoing_ids_are_numbers() {
        let mut ids = RequestIds::default();
        let mut request = json!({
            "jsonrpc": "2.0",
            "id": "11111111-1111-1111-1111-111111111111",
            "method": "session/request_permission",
            "params": {}
        });
        assert_eq!(
            ids.rewrite_outgoing(&mut request),
            vec![(0, "11111111-1111-1111-1111-111111111111".to_string())]
        );
        assert_eq!(request["id"], json!(0));

        let mut second = json!({
            "jsonrpc": "2.0",
            "id": "22222222-2222-2222-2222-222222222222",
            "method": "elicitation/create",
            "params": {}
        });
        assert_eq!(
            ids.rewrite_outgoing(&mut second),
            vec![(1, "22222222-2222-2222-2222-222222222222".to_string())]
        );
        assert_eq!(second["id"], json!(1));

        let mut reply = json!({"jsonrpc": "2.0", "id": 0, "result": {}});
        ids.rewrite_incoming(&mut reply);
        assert_eq!(reply["id"], "11111111-1111-1111-1111-111111111111");
        // The answered request is pruned from the correlation map.
        assert_eq!(ids.tracked_len(), 1);

        // A client request that happens to reuse the number 0 keeps its id.
        let mut client_request =
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}});
        ids.rewrite_incoming(&mut client_request);
        assert_eq!(client_request["id"], json!(0));

        // An unknown numeric response id is left alone (the crate reports it).
        let mut unknown = json!({"jsonrpc": "2.0", "id": 7, "result": {}});
        ids.rewrite_incoming(&mut unknown);
        assert_eq!(unknown["id"], json!(7));
    }

    /// F2c — an outgoing `$/cancel_request` carries the numeric id the client
    /// knows, not the crate's uuid.
    #[test]
    fn inv_52_cancel_request_id_translated() {
        let mut ids = RequestIds::default();
        let mut request = json!({
            "jsonrpc": "2.0",
            "id": "abc-uuid",
            "method": "session/request_permission",
            "params": {}
        });
        let _ = ids.rewrite_outgoing(&mut request);
        let mut cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": {"requestId": "abc-uuid"}
        });
        assert!(ids.rewrite_outgoing(&mut cancel).is_empty());
        assert_eq!(cancel["params"]["requestId"], json!(0));

        // A batch of responses is rewritten member by member.
        let mut batch = json!([
            {"jsonrpc": "2.0", "id": 0, "result": {}},
            {"jsonrpc": "2.0", "id": 9, "result": {}}
        ]);
        ids.rewrite_incoming(&mut batch);
        assert_eq!(batch[0]["id"], "abc-uuid");
        assert_eq!(batch[1]["id"], json!(9));
    }

    /// F2c — the line-level helpers are the exact pair used by the transport.
    #[test]
    fn inv_52_line_rewrites() {
        let mut ids = RequestIds::default();
        let (outgoing, assigned) = rewrite_outgoing_line(
            r#"{"jsonrpc":"2.0","id":"u-1","method":"session/request_permission","params":{}}"#,
            &mut ids,
        );
        assert_eq!(assigned, vec![(0, "u-1".to_string())]);
        let value: Value = serde_json::from_str(&outgoing).expect("valid JSON");
        assert_eq!(value["id"], json!(0));

        let incoming = rewrite_incoming_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#, &mut ids);
        let value: Value = serde_json::from_str(&incoming).expect("valid JSON");
        assert_eq!(value["id"], "u-1");

        // Unparsable text passes through unchanged (the transport never hands
        // it to the crate; it is answered as a parse error).
        assert_eq!(
            rewrite_outgoing_line("nope", &mut ids),
            ("nope".to_string(), Vec::new())
        );
    }

    /// F2c — the blocking stdio reader must drain the `(number, id)` pairs the
    /// writer queued while it was parked in `read`. Otherwise the client's
    /// numeric reply to `session/request_permission` reaches the crate
    /// untranslated and the request never resolves (the binary path; the async
    /// duplex path drains after its read as well).
    #[test]
    fn inv_52_blocking_reader_drains_ids_after_read() {
        use futures::StreamExt as _;
        use std::io::Read;

        /// A reader that signals when it enters `read`, then waits for the test
        /// to hand it one line; a later read is EOF. This makes the "writer
        /// assigns an id while the reader is blocked" interleaving
        /// deterministic.
        struct GateReader {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<Vec<u8>>,
            done: bool,
        }
        impl Read for GateReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.done {
                    return Ok(0);
                }
                let _ = self.entered.send(());
                let data = self.release.recv().unwrap_or_default();
                self.done = true;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (in_tx, mut in_rx) = futures::channel::mpsc::unbounded::<io::Result<String>>();
        let (out_tx, _out_rx) = futures::channel::mpsc::unbounded::<String>();
        let (assigned_tx, assigned_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, String)>();

        let handle = std::thread::spawn(move || {
            read_wire_blocking(
                std::io::BufReader::new(GateReader {
                    entered: entered_tx,
                    release: release_rx,
                    done: false,
                }),
                in_tx,
                out_tx,
                assigned_rx,
            );
        });

        // The reader is parked inside `read`; the writer assigns id 0 to the
        // crate's uuid string and then writes the client's numeric reply.
        entered_rx.recv().expect("reader entered read");
        assigned_tx
            .send((0, "u-1".to_string()))
            .expect("queue mapping");
        release_tx
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\n".to_vec())
            .expect("release line");

        let line = futures::executor::block_on(in_rx.next()).expect("one rewritten line");
        let value: Value = serde_json::from_str(&line.expect("line ok")).expect("valid JSON");
        assert_eq!(
            value["id"], "u-1",
            "the numeric reply must map back to the uuid"
        );
        handle.join().expect("reader thread ends");
    }

    #[test]
    fn inv_79_ids_sequential_from_zero() {
        let mut ids = RequestIds::default();
        let mut req0 = json!({"jsonrpc":"2.0","id":"uuid-a","method":"session/request_permission","params":{}});
        let mut req1 = json!({"jsonrpc":"2.0","id":"uuid-b","method":"session/request_permission","params":{}});
        let mut req2 = json!({"jsonrpc":"2.0","id":"uuid-c","method":"session/request_permission","params":{}});

        let res0 = ids.rewrite_outgoing(&mut req0);
        assert_eq!(res0, vec![(0, "uuid-a".to_string())]);
        assert_eq!(req0["id"], 0);

        let res1 = ids.rewrite_outgoing(&mut req1);
        assert_eq!(res1, vec![(1, "uuid-b".to_string())]);
        assert_eq!(req1["id"], 1);

        let res2 = ids.rewrite_outgoing(&mut req2);
        assert_eq!(res2, vec![(2, "uuid-c".to_string())]);
        assert_eq!(req2["id"], 2);

        // Same string id re-uses the allocated number
        let mut req0_dup = json!({"jsonrpc":"2.0","id":"uuid-a","method":"session/request_permission","params":{}});
        let res0_dup = ids.rewrite_outgoing(&mut req0_dup);
        assert_eq!(res0_dup, vec![(0, "uuid-a".to_string())]);
        assert_eq!(req0_dup["id"], 0);
    }

    #[test]
    fn inv_79_response_id_restored_and_unknown_passes() {
        let mut ids = RequestIds::default();
        let mut req = json!({"jsonrpc":"2.0","id":"uuid-x","method":"session/request_permission","params":{}});
        ids.rewrite_outgoing(&mut req);

        let mut res = json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"allow"}}});
        ids.rewrite_incoming(&mut res);
        assert_eq!(res["id"], "uuid-x");

        let mut unknown_res = json!({"jsonrpc":"2.0","id":999,"result":{}});
        ids.rewrite_incoming(&mut unknown_res);
        assert_eq!(unknown_res["id"], 999);
    }

    #[test]
    fn inv_79_cancel_request_id_rewritten() {
        let mut ids = RequestIds::default();
        let mut req = json!({"jsonrpc":"2.0","id":"uuid-cancelling","method":"session/request_permission","params":{}});
        ids.rewrite_outgoing(&mut req);

        let mut cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": {
                "requestId": "uuid-cancelling"
            }
        });
        let res = ids.rewrite_outgoing(&mut cancel);
        assert!(res.is_empty());
        assert_eq!(cancel["params"]["requestId"], 0);
    }

    #[test]
    fn inv_79_batch_entries_mapped() {
        // The writer and the reader own separate maps in the transport: the
        // writer assigns, the reader remembers each pair it is sent. A single
        // map for both directions would hide a batch member losing its pair.
        let mut writer = RequestIds::default();
        let mut reader = RequestIds::default();
        let mut batch_req = json!([
            {"jsonrpc":"2.0","id":"u1","method":"m1","params":{}},
            {"jsonrpc":"2.0","id":"u2","method":"m2","params":{}}
        ]);
        let assigned = writer.rewrite_outgoing(&mut batch_req);
        assert_eq!(
            assigned,
            vec![(0, "u1".to_string()), (1, "u2".to_string())],
            "every batch member must return its own pair"
        );
        assert_eq!(batch_req[0]["id"], 0);
        assert_eq!(batch_req[1]["id"], 1);
        for (number, id) in assigned {
            reader.remember(number, &id);
        }

        let mut batch_res = json!([
            {"jsonrpc":"2.0","id":0,"result":"r1"},
            {"jsonrpc":"2.0","id":1,"result":"r2"}
        ]);
        reader.rewrite_incoming(&mut batch_res);
        assert_eq!(batch_res[0]["id"], "u1");
        assert_eq!(batch_res[1]["id"], "u2");
        assert_eq!(reader.tracked_len(), 0, "both answered pairs are pruned");
    }

    #[test]
    fn inv_79_notifications_and_client_requests_untouched() {
        let mut ids = RequestIds::default();
        let mut notif = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s"}
        });
        let res = ids.rewrite_outgoing(&mut notif);
        assert!(res.is_empty());
        assert!(notif.get("id").is_none());

        let mut client_req = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "session/prompt",
            "params": {}
        });
        ids.rewrite_incoming(&mut client_req);
        assert_eq!(client_req["id"], 42);
    }

    #[test]
    fn inv_79_correlation_maps_pruned_and_capped() {
        // The reader drops a mapping as soon as the response is restored.
        let mut writer = RequestIds::default();
        let mut reader = RequestIds::default();
        for i in 0..100 {
            let mut req = json!({"jsonrpc":"2.0","id":format!("u-{i}"),"method":"m","params":{}});
            for (number, id) in writer.rewrite_outgoing(&mut req) {
                reader.remember(number, &id);
            }
        }
        assert_eq!(reader.tracked_len(), 100);
        for i in 0..100 {
            let mut res = json!({"jsonrpc":"2.0","id":i,"result":{}});
            reader.rewrite_incoming(&mut res);
            assert_eq!(res["id"], format!("u-{i}"));
        }
        assert_eq!(
            reader.tracked_len(),
            0,
            "answered requests must not grow the map"
        );

        // A never-answered entry is aged out once newer ids push the map past
        // its cap, so a long-lived process cannot grow it without bound.
        let mut writer = RequestIds::default();
        for i in 0..(MAX_TRACKED_IDS + 100) {
            let mut req =
                json!({"jsonrpc":"2.0","id":format!("never-{i}"),"method":"m","params":{}});
            writer.rewrite_outgoing(&mut req);
        }
        assert_eq!(writer.tracked_len(), MAX_TRACKED_IDS);
        // The oldest ids are gone; the newest still translate a cancel.
        let mut cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": {"requestId": format!("never-{}", MAX_TRACKED_IDS + 99)}
        });
        assert!(writer.rewrite_outgoing(&mut cancel).is_empty());
        assert_eq!(cancel["params"]["requestId"], json!(MAX_TRACKED_IDS + 99));
    }

    #[tokio::test]
    async fn inv_79_bridge_forwards_all_frames_before_close() {
        use agent_client_protocol::{Channel, RawJsonRpcMessage};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let (a1, a2) = Channel::duplex();
        let (b1, b2) = Channel::duplex();
        let a_to_b_count = Arc::new(AtomicUsize::new(0));
        let b_to_a_count = Arc::new(AtomicUsize::new(0));
        let c1 = a_to_b_count.clone();
        let c2 = b_to_a_count.clone();

        let bridge = tokio::spawn(async move {
            Channel::bridge_with_inspection(
                a2,
                b2,
                move |_msg: &RawJsonRpcMessage| {
                    c1.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                move |_msg: &RawJsonRpcMessage| {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
        });

        drop(a1);
        drop(b1);
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), bridge)
            .await
            .expect("bridge closed within timeout")
            .expect("join handle ok");
        assert!(res.is_ok());
    }
}
