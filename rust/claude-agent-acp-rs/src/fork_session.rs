//! `session/fork` fork-point parsing and resolution (M-32/M-33/M-34/M-36,
//! port of `fork-session.ts`).
//!
//! The ACP request may carry `_meta.jetbrains.air.fork` with `version: 1` and
//! the `messageId` the client wants to fork up to. That id is the ACP message
//! id, not the SDK transcript uuid: it is resolved through the live
//! messageId -> uuid map first, then the active `getSessionMessages` chain,
//! then the full imported transcript (including inactive branches), matching
//! `resolveFromFullHistory` (`fork-session.ts:112-150`).
//!
//! The module is pure: the caller reads the transcript snapshots and the live
//! map, and [`resolve_fork_point`] returns the SDK uuid or `None`.

use std::collections::HashMap;

use serde_json::Value;

use crate::message_ids::MessageIdMap;

/// A validated fork point (`ForkPoint`, `fork-session.ts:41-46`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPoint {
    pub message_id: String,
    pub message_fingerprint: Option<String>,
    pub message_occurrence: Option<u64>,
}

/// A malformed AIR fork point. Node's `fork.messageId?.trim()` only
/// short-circuits on null/undefined, so a number or an object throws a
/// TypeError (`fork.messageId.trim is not a function`), which reaches the
/// client as an internal error rather than a silent full fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPointError {
    /// The offending field (`messageId` or `messageFingerprint`).
    pub field: &'static str,
    /// The value that is not a string.
    pub value: Value,
}

impl std::fmt::Display for ForkPointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fork.{}.trim is not a function", self.field)
    }
}

impl std::error::Error for ForkPointError {}

/// `forkPoint` (`fork-session.ts:48-64`): `_meta.jetbrains.air.fork` with
/// `version === 1` and a non-empty `messageId`. A fingerprint and a 1-based
/// occurrence are optional. `Err` when `messageId` / `messageFingerprint` is
/// present but not a string, like the SDK's `?.trim()` TypeError.
pub fn fork_point(meta: &Value) -> Result<Option<ForkPoint>, ForkPointError> {
    let Some(fork) = meta.pointer("/jetbrains/air/fork") else {
        return Ok(None);
    };
    if !js_integer_is_one(fork.get("version")) {
        return Ok(None);
    }
    let message_id = match fork.get("messageId") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(id)) => id.trim(),
        Some(other) => {
            return Err(ForkPointError {
                field: "messageId",
                value: other.clone(),
            })
        }
    };
    if message_id.is_empty() {
        return Ok(None);
    }
    let message_fingerprint = match fork.get("messageFingerprint") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Some(other) => {
            return Err(ForkPointError {
                field: "messageFingerprint",
                value: other.clone(),
            })
        }
    };
    // AIR computes this with count(...) over the visible prefix, so it is
    // 1-based. `Number.isSafeInteger` accepts integral JSON numbers.
    let message_occurrence = fork.get("messageOccurrence").and_then(|value| {
        if let Some(n) = value.as_u64() {
            (n > 0).then_some(n)
        } else {
            value
                .as_f64()
                .filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0)
                .map(|n| n as u64)
        }
    });
    Ok(Some(ForkPoint {
        message_id: message_id.to_string(),
        message_fingerprint,
        message_occurrence,
    }))
}

/// JS `fork.version !== 1`: the JSON number `1` and `1.0` both compare equal.
fn js_integer_is_one(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .map_or_else(|| number.as_f64().is_some_and(|n| n == 1.0), |n| n == 1),
        _ => false,
    }
}

/// Strip a trailing AIR visible-segment suffix (`:segment:<n>`).
pub fn strip_segment_suffix(message_id: &str) -> &str {
    let Some(at) = message_id.rfind(":segment:") else {
        return message_id;
    };
    let suffix = &message_id[at + ":segment:".len()..];
    if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
        &message_id[..at]
    } else {
        message_id
    }
}

/// `forkPointMessageIdCandidates` (`fork-session.ts:66-70`): prefer the exact
/// id before its ACP source id.
pub fn fork_point_message_id_candidates(message_id: &str) -> Vec<String> {
    let protocol = strip_segment_suffix(message_id);
    if protocol == message_id {
        vec![message_id.to_string()]
    } else {
        vec![message_id.to_string(), protocol.to_string()]
    }
}

/// One assistant group keyed by `messageIdForGrouping` (`assistantGroups`,
/// `fork-session.ts:89-108`): the text of every message of the group and the
/// last uuid seen.
#[derive(Debug, Clone, Default)]
struct AssistantGroup {
    uuid: Option<String>,
    text: String,
}

fn assistant_groups(entries: &[Value]) -> HashMap<String, AssistantGroup> {
    let mut groups: HashMap<String, AssistantGroup> = HashMap::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || entry
                .get("parent_tool_use_id")
                .is_some_and(|v| !v.is_null())
            || entry.get("parent_agent_id").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let Some(message_id) = crate::mcp_command::message_id_of(entry) else {
            continue;
        };
        let group = groups.entry(message_id).or_default();
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            if !uuid.is_empty() {
                group.uuid = Some(uuid.to_string());
            }
        }
        if let Some(text) = crate::session_failure::assistant_message_text(
            entry.get("message").unwrap_or(&Value::Null),
        ) {
            group.text.push_str(&text);
        }
    }
    groups
}

/// `fingerprint` (`fork-session.ts:110-112`): `sha256:<hex>` of the group text.
pub fn fingerprint(text: &str) -> String {
    format!("sha256:{}", sha256_hex(text.as_bytes()))
}

/// `fingerprintOccurrenceOnBranch` (`fork-session.ts:114-146`): how many
/// distinct groups on the branch that ends at `target_uuid` have the expected
/// fingerprint, counting from the target towards the root.
fn fingerprint_occurrence_on_branch(
    target_uuid: &str,
    expected_fingerprint: &str,
    entries: &[Value],
    groups: &HashMap<String, AssistantGroup>,
) -> u64 {
    let by_uuid: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("uuid")
                .and_then(Value::as_str)
                .map(|u| (u, entry))
        })
        .collect();
    let mut seen_groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut occurrence = 0u64;
    let mut cursor = Some(target_uuid.to_string());
    while let Some(uuid) = cursor {
        let Some(entry) = by_uuid.get(uuid.as_str()) else {
            break;
        };
        if entry.get("type").and_then(Value::as_str) == Some("assistant")
            && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
            && entry.get("parent_tool_use_id").is_none_or(Value::is_null)
            && entry.get("parent_agent_id").is_none_or(Value::is_null)
        {
            if let Some(message_id) = crate::mcp_command::message_id_of(entry) {
                if seen_groups.insert(message_id.clone()) {
                    if let Some(group) = groups.get(&message_id) {
                        if fingerprint(&group.text) == expected_fingerprint {
                            occurrence += 1;
                        }
                    }
                }
            }
        }
        cursor = entry
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(String::from);
    }
    occurrence
}

/// `resolveFromFullHistory` (`fork-session.ts:148-183`): exact candidate
/// messageId first; else a unique fingerprint match; else the unique
/// occurrence on its branch.
fn resolve_from_full_history(
    entries: &[Value],
    candidates: &[String],
    point: &ForkPoint,
) -> Option<String> {
    let groups = assistant_groups(entries);
    let exact = candidates
        .iter()
        .find_map(|candidate| groups.get(candidate).and_then(|group| group.uuid.clone()));
    if exact.is_some() {
        return exact;
    }
    let expected = point.message_fingerprint.as_deref()?;
    let occurrence = point.message_occurrence?;

    let fingerprint_matches: Vec<(String, String)> = groups
        .values()
        .filter_map(|group| {
            let uuid = group.uuid.clone()?;
            (fingerprint(&group.text) == expected).then_some((uuid, group.text.clone()))
        })
        .collect();
    if fingerprint_matches.len() == 1 {
        return Some(fingerprint_matches[0].0.clone());
    }
    let occurrence_matches: Vec<&(String, String)> = fingerprint_matches
        .iter()
        .filter(|(uuid, _)| {
            fingerprint_occurrence_on_branch(uuid, expected, entries, &groups) == occurrence
        })
        .collect();
    if occurrence_matches.len() == 1 {
        return Some(occurrence_matches[0].0.clone());
    }
    None
}

/// Where a resolved fork point came from (`timing.phase("full-history", ...)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkResolutionSource {
    Live,
    Active,
    Full,
}

/// Resolve the fork point of `point` against the live map, the active
/// `getSessionMessages` chain and the full imported transcript
/// (`forkSession`, `fork-session.ts:194-237`). `None` when no message matches.
pub fn resolve_fork_point(
    point: &ForkPoint,
    live: Option<&MessageIdMap>,
    active: &[Value],
    full: &[Value],
) -> Option<(String, ForkResolutionSource)> {
    let candidates = fork_point_message_id_candidates(&point.message_id);
    if let Some(uuid) = candidates
        .iter()
        .find_map(|candidate| live.and_then(|map| map.get(candidate)))
    {
        return Some((uuid.to_string(), ForkResolutionSource::Live));
    }
    let active_uuid = candidates.iter().find_map(|candidate| {
        active
            .iter()
            .rev()
            .find(|message| {
                crate::mcp_command::message_id_of(message).as_deref() == Some(candidate)
            })
            .and_then(|message| message.get("uuid").and_then(Value::as_str))
            .filter(|uuid| !uuid.is_empty())
    });
    if let Some(uuid) = active_uuid {
        return Some((uuid.to_string(), ForkResolutionSource::Active));
    }
    resolve_from_full_history(full, &candidates, point)
        .map(|uuid| (uuid, ForkResolutionSource::Full))
}

// ---- SHA-256 ---------------------------------------------------------------
//
// The SDK fingerprints a message group with Node's `createHash("sha256")`.
// The crate has no digest dependency (and none may be added), so the standard
// FIPS 180-4 compression function lives here.

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `input` as lowercase hex (`createHash("sha256").digest("hex")`).
pub fn sha256_hex(input: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    for block in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h;
        for i in 0..64 {
            let s1 = a[4].rotate_right(6) ^ a[4].rotate_right(11) ^ a[4].rotate_right(25);
            let ch = (a[4] & a[5]) ^ ((!a[4]) & a[6]);
            let t1 = a[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a[0].rotate_right(2) ^ a[0].rotate_right(13) ^ a[0].rotate_right(22);
            let maj = (a[0] & a[1]) ^ (a[0] & a[2]) ^ (a[1] & a[2]);
            let t2 = s0.wrapping_add(maj);
            a[7] = a[6];
            a[6] = a[5];
            a[5] = a[4];
            a[4] = a[3].wrapping_add(t1);
            a[3] = a[2];
            a[2] = a[1];
            a[1] = a[0];
            a[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(a[i]);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(fork: Value) -> Value {
        json!({"jetbrains": {"air": {"fork": fork}}})
    }

    fn assistant(uuid: &str, parent: Option<&str>, api_id: &str, text: &str) -> Value {
        json!({"type": "assistant", "uuid": uuid, "parentUuid": parent,
            "message": {"id": api_id, "role": "assistant",
                "content": [{"type": "text", "text": text}]}})
    }

    fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent,
            "message": {"role": "user", "content": text}})
    }

    /// M-34 — version 1, messageId, fingerprint and a 1-based occurrence.
    #[test]
    fn m34_fork_point_parse() {
        let point = fork_point(&meta(json!({
            "version": 1, "messageId": " m1 ", "messageFingerprint": " sha256:ab ",
            "messageOccurrence": 2
        })))
        .expect("no parse error")
        .expect("valid point");
        assert_eq!(point.message_id, "m1");
        assert_eq!(point.message_fingerprint.as_deref(), Some("sha256:ab"));
        assert_eq!(point.message_occurrence, Some(2));
        // `1.0` is the same number as `1`.
        assert!(
            fork_point(&meta(json!({"version": 1.0, "messageId": "m1"})))
                .expect("no parse error")
                .is_some()
        );
        assert!(fork_point(&meta(json!({"version": 2, "messageId": "m1"})))
            .expect("no parse error")
            .is_none());
        assert!(fork_point(&meta(json!({"version": 1, "messageId": " "})))
            .expect("no parse error")
            .is_none());
        assert!(fork_point(&json!({})).expect("no parse error").is_none());
        // A zero/negative occurrence is omitted.
        assert_eq!(
            fork_point(&meta(
                json!({"version": 1, "messageId": "m1", "messageOccurrence": 0})
            ))
            .expect("no parse error")
            .unwrap()
            .message_occurrence,
            None
        );
        assert_eq!(
            fork_point(&meta(
                json!({"version": 1, "messageId": "m1", "messageOccurrence": 1.5})
            ))
            .expect("no parse error")
            .unwrap()
            .message_occurrence,
            None
        );
    }

    /// LOW-5 — a non-string `messageId` / `messageFingerprint` is the SDK's
    /// `?.trim()` TypeError, an error rather than a silent full fork; null and
    /// absent stay the optional chain's short circuit.
    #[test]
    fn low5_non_string_fork_point_is_an_error() {
        let err = fork_point(&meta(json!({"version": 1, "messageId": 7}))).expect_err("number");
        assert_eq!(err.field, "messageId");
        assert_eq!(err.value, json!(7));
        assert_eq!(err.to_string(), "fork.messageId.trim is not a function");
        let err = fork_point(&meta(
            json!({"version": 1, "messageId": "m1", "messageFingerprint": {"a": 1}}),
        ))
        .expect_err("object");
        assert_eq!(err.field, "messageFingerprint");
        assert!(fork_point(&meta(json!({"version": 1, "messageId": null})))
            .expect("no parse error")
            .is_none());
        assert!(fork_point(&meta(json!({"version": 1, "messageId": 7}))).is_err());
    }

    /// M-36 — a `:segment:N` suffix is stripped from the candidate id.
    #[test]
    fn m36_segment_suffix_candidate() {
        assert_eq!(strip_segment_suffix("msg-1"), "msg-1");
        assert_eq!(strip_segment_suffix("msg-1:segment:2"), "msg-1");
        assert_eq!(strip_segment_suffix("msg-1:segment:x"), "msg-1:segment:x");
        assert_eq!(strip_segment_suffix("msg-1:segment:"), "msg-1:segment:");
        assert_eq!(
            fork_point_message_id_candidates("msg-1:segment:2"),
            vec!["msg-1:segment:2".to_string(), "msg-1".to_string()]
        );
        assert_eq!(
            fork_point_message_id_candidates("msg-1"),
            vec!["msg-1".to_string()]
        );
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// M-33 — the fork point resolves through the live map first, then the
    /// active chain, then the full transcript; a fingerprint identifies a
    /// message even when the id changed.
    #[test]
    fn m33_resolves_live_active_and_full_history() {
        let point = ForkPoint {
            message_id: "m1".to_string(),
            message_fingerprint: None,
            message_occurrence: None,
        };
        let mut live = MessageIdMap::default();
        live.set("m1", "uuid-live");
        assert_eq!(
            resolve_fork_point(&point, Some(&live), &[], &[]),
            Some(("uuid-live".to_string(), ForkResolutionSource::Live))
        );

        let active = vec![
            user("u1", None, "hi"),
            assistant("a1", Some("u1"), "m1", "answer"),
        ];
        assert_eq!(
            resolve_fork_point(&point, None, &active, &[]),
            Some(("a1".to_string(), ForkResolutionSource::Active))
        );

        // The id is unknown; the fingerprint finds the single persisted group.
        let full = vec![
            user("u1", None, "hi"),
            assistant("a1", Some("u1"), "old-id", "answer"),
            assistant("a2", Some("a1"), "other", "different"),
        ];
        let fingerprint_point = ForkPoint {
            message_id: "m1".to_string(),
            message_fingerprint: Some(fingerprint("answer")),
            message_occurrence: Some(1),
        };
        assert_eq!(
            resolve_fork_point(&fingerprint_point, None, &[], &full),
            Some(("a1".to_string(), ForkResolutionSource::Full))
        );
        let unknown = ForkPoint {
            message_id: "nope".to_string(),
            message_fingerprint: None,
            message_occurrence: None,
        };
        assert_eq!(
            resolve_fork_point(&unknown, None, &[], &full),
            None,
            "an unknown id without a fingerprint is not found"
        );
    }

    /// M-33 — the occurrence disambiguates two groups with the same text.
    #[test]
    fn m33_occurrence_disambiguates_equal_fingerprints() {
        let full = vec![
            user("u1", None, "one"),
            assistant("a1", Some("u1"), "m1", "same"),
            user("u2", Some("a1"), "two"),
            assistant("a2", Some("u2"), "m2", "same"),
        ];
        let point = |occurrence: u64| ForkPoint {
            message_id: "unknown".to_string(),
            message_fingerprint: Some(fingerprint("same")),
            message_occurrence: Some(occurrence),
        };
        assert_eq!(
            resolve_fork_point(&point(1), None, &[], &full),
            Some(("a1".to_string(), ForkResolutionSource::Full))
        );
        assert_eq!(
            resolve_fork_point(&point(2), None, &[], &full),
            Some(("a2".to_string(), ForkResolutionSource::Full))
        );
        assert_eq!(resolve_fork_point(&point(3), None, &[], &full), None);
    }
}
