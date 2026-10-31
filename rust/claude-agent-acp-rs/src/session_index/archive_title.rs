//! The session archive, in the on-disk format of JetBrains AIR's own Claude
//! integration (upstream `session-index/archive-title.ts`): a session is
//! archived when its title carries the `[archived] ` prefix, and archive,
//! unarchive and rename append the title as a pair of records to the
//! transcript:
//!
//! ```text
//! {"type":"custom-title","customTitle":"[archived] Title","sessionId":"…"}
//! {"type":"agent-name","agentName":"[archived] Title","sessionId":"…"}
//! ```
//!
//! The archive state follows AIR's native Claude provider ([`effective_title`]):
//! the last agent name decides, else the custom title the SDK reports.

use super::js_text::{cut_utf16, is_ascii_ws, is_js_ws, js_trim, utf16_len};

/// The title prefix of an archived session.
pub const ARCHIVED_TITLE_PREFIX: &str = "[archived] ";

/// What the CLI keeps of a stored title: `title.slice(0, 200).trim()`.
const CLI_TITLE_LIMIT: usize = 200;

const MARKER: &str = "[archived]";

/// The length of one archive prefix at the start of `title`, or `None`:
/// leading space (what a trim removes), the marker, a run of ASCII white
/// space, then a title character (`ARCHIVED_PATTERN`, the lookahead excluded).
fn prefix_len(title: &str) -> Option<usize> {
    let after_space = title.len() - title.trim_start_matches(is_js_ws).len();
    let rest = title[after_space..].strip_prefix(MARKER)?;
    let run = rest.len() - rest.trim_start_matches(is_ascii_ws).len();
    if run == 0 || run == rest.len() {
        // No white space after the marker, or nothing after the white space
        // (a regex backtracks into the run, and what remains is white space).
        return None;
    }
    Some(after_space + MARKER.len() + run)
}

/// Whitespace collapsed to single spaces and trimmed, as AIR stores a title.
pub fn normalize_stored_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut in_run = false;
    for c in title.chars() {
        if is_ascii_ws(c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            in_run = false;
            out.push(c);
        }
    }
    js_trim(&out).to_string()
}

/// Cut to what the CLI keeps of a title.
fn cap_title(title: &str) -> String {
    if utf16_len(title) <= CLI_TITLE_LIMIT {
        title.to_string()
    } else {
        js_trim(cut_utf16(title, CLI_TITLE_LIMIT)).to_string()
    }
}

/// Whether a session name (see [`effective_title`]) marks its session archived.
pub fn is_archived_title(title: Option<&str>) -> bool {
    title.is_some_and(|t| prefix_len(t).is_some())
}

/// The title shown for a stored title: without the archive prefix, removed
/// once, as AIR shows it.
pub fn visible_title(title: &str) -> String {
    match prefix_len(title) {
        Some(len) => title[len..].to_string(),
        None => title.to_string(),
    }
}

/// The title of a session that has none at all, as AIR names it.
pub fn default_session_title(session_id: &str) -> String {
    format!("Session {}", cut_utf16(session_id, 8))
}

/// Every leading archive prefix removed (`ARCHIVED_PREFIXES`).
fn strip_all_prefixes(title: &str) -> &str {
    let mut rest = title;
    while let Some(len) = prefix_len(rest) {
        rest = &rest[len..];
    }
    rest
}

/// The title that `title` is stored as in the given archive state: the visible
/// title, with the prefix when archived, normalized and cut to the CLI limit.
pub fn stored_title(title: &str, archived: bool, session_id: &str) -> String {
    // Every prefix: a title stored twice prefixed must not stay archived.
    let normalized = normalize_stored_title(strip_all_prefixes(title));
    let visible = if normalized.is_empty() {
        default_session_title(session_id)
    } else {
        normalized
    };
    cap_title(&if archived {
        format!("{ARCHIVED_TITLE_PREFIX}{visible}")
    } else {
        visible
    })
}

/// The SDK titles of a transcript (`SDKSessionInfo.customTitle` / `.summary`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SdkTitles {
    /// The title the CLI or a client set.
    pub custom_title: Option<String>,
    /// The custom title, else a generated one.
    pub summary: Option<String>,
}

/// The title of a session and its archive state, by the rule of AIR's native
/// Claude provider: the session's name is its last agent name, else its custom
/// title, and the session is archived exactly when that name starts with the
/// prefix. The title is that agent name, else `summary`.
pub fn effective_title(agent_name: Option<&str>, titles: &SdkTitles) -> (Option<String>, bool) {
    let title = agent_name
        .map(str::to_string)
        .or_else(|| titles.summary.clone());
    let archived = is_archived_title(agent_name.or(titles.custom_title.as_deref()));
    (title, archived)
}

/// `JSON.stringify` of a string.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// The `custom-title` and `agent-name` records of `title`, one per line.
pub fn title_records(session_id: &str, title: &str) -> String {
    format!(
        "{{\"type\":\"custom-title\",\"customTitle\":{},\"sessionId\":{}}}\n{{\"type\":\"agent-name\",\"agentName\":{},\"sessionId\":{}}}\n",
        json_string(title),
        json_string(session_id),
        json_string(title),
        json_string(session_id),
    )
}

/// The `agent-name` record alone.
pub fn agent_name_record(session_id: &str, title: &str) -> String {
    format!(
        "{{\"type\":\"agent-name\",\"agentName\":{},\"sessionId\":{}}}",
        json_string(title),
        json_string(session_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    /// archive titles > tells an archived title by its prefix once normalized
    #[test]
    fn tells_an_archived_title_by_its_prefix() {
        assert!(is_archived_title(Some("[archived] x")));
        assert!(is_archived_title(Some("  [archived]\t\nx")));
        assert!(is_archived_title(Some("\u{a0}[archived] \u{a0}")));
        assert!(!is_archived_title(Some("[archived]")));
        assert!(!is_archived_title(Some("[archived] ")));
        assert!(!is_archived_title(Some("[archived]x")));
        assert!(!is_archived_title(Some("x [archived] y")));
        assert!(!is_archived_title(None));
        assert_eq!(visible_title("[archived]  x y"), "x y");
        assert_eq!(visible_title("[archived] [archived] x"), "[archived] x");
        assert_eq!(visible_title("plain"), "plain");
    }

    /// archive titles > stores the visible title normalized, with the prefix when archived, cut to 200
    #[test]
    fn stores_the_visible_title_normalized_and_cut() {
        assert_eq!(stored_title("  a \n b ", false, ID), "a b");
        assert_eq!(stored_title("a b", true, ID), "[archived] a b");
        // Every prefix goes: a title stored twice prefixed does not stay archived.
        assert_eq!(stored_title("[archived] [archived] a", false, ID), "a");
        assert_eq!(
            stored_title("[archived] [archived] a", true, ID),
            "[archived] a"
        );
        assert_eq!(stored_title("", true, ID), "[archived] Session aaaaaaaa");
        assert_eq!(stored_title("   ", false, ID), "Session aaaaaaaa");
        let long = "x".repeat(300);
        let stored = stored_title(&long, true, ID);
        assert_eq!(stored.len(), 200);
        assert!(stored.starts_with("[archived] x"));
        // The upstream cases.
        let id = "0b0e6c2e-1d55-4c1f-9b4e-2b1d7f6f1a11";
        assert_eq!(
            stored_title(" Fix\n the  parser ", true, id),
            "[archived] Fix the parser"
        );
        assert_eq!(stored_title("[archived] Fix it", false, id), "Fix it");
        assert_eq!(
            stored_title("[archived] Fix it", true, id),
            "[archived] Fix it"
        );
        assert_eq!(stored_title("", true, id), "[archived] Session 0b0e6c2e");
        assert_eq!(
            stored_title(&format!("A{}B", " ".repeat(5000)), true, id),
            "[archived] A B"
        );
        assert_eq!(
            stored_title(&format!("{}X", "[archived] ".repeat(10_000)), true, id),
            "[archived] X"
        );
        assert_eq!(
            stored_title(&format!("{} {}", "a".repeat(188), "b".repeat(20)), true, id),
            format!("[archived] {}", "a".repeat(188))
        );
    }

    /// archive titles > writes the custom-title and agent-name records with AIR's fields
    #[test]
    fn writes_the_records_with_airs_fields() {
        assert_eq!(
            title_records(ID, "[archived] T \"q\""),
            format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"[archived] T \\\"q\\\"\",\"sessionId\":\"{ID}\"}}\n{{\"type\":\"agent-name\",\"agentName\":\"[archived] T \\\"q\\\"\",\"sessionId\":\"{ID}\"}}\n"
            )
        );
    }

    /// archive titles > decides the archive state by the agent name first, else the custom title
    #[test]
    fn the_agent_name_decides_the_archive_state_first() {
        let titles = SdkTitles {
            custom_title: Some("[archived] c".into()),
            summary: Some("[archived] c".into()),
        };
        assert_eq!(
            effective_title(Some("plain"), &titles),
            (Some("plain".into()), false)
        );
        assert_eq!(
            effective_title(None, &titles),
            (Some("[archived] c".into()), true)
        );
        // A generated summary never archives a session.
        let generated = SdkTitles {
            custom_title: None,
            summary: Some("[archived] g".into()),
        };
        assert_eq!(
            effective_title(None, &generated),
            (Some("[archived] g".into()), false)
        );
    }
}
