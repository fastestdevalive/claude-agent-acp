//! Port of upstream `src/prompt-suggestions.ts` (pure).
//!
//! With `promptSuggestions` on, the CLI emits one `prompt_suggestion` message
//! after a turn's `result` holding the prompt it expects the user to send next.
//! ACP has no field for it, so it travels as
//! `session_info_update._meta.promptSuggestions`: a client that does not read
//! the key sees a session info update that says nothing and ignores it.
//!
//! The CLI sends no suggestion on the first turn of a session, after an API
//! error, in plan mode, and at the account's usage limit, so most sessions have
//! turns that end with no prediction at all.

use serde_json::{json, Value};

/// The `_meta` key the suggestion travels under. It names no vendor.
pub const PROMPT_SUGGESTIONS_META_KEY: &str = "promptSuggestions";

/// The suggestion as it goes on the wire, or `None` for a suggestion that says
/// nothing: the CLI can predict whitespace, and an empty text would otherwise
/// reach the composer as a hint with no content.
pub fn prompt_suggestion_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The `_meta` of the session info update that publishes `text`. `None` sends an
/// empty `suggestions` list, which clears the prompt the client holds — the turn
/// predicted nothing, so the composer must not keep the previous prediction.
pub fn prompt_suggestions_meta(text: Option<&str>) -> Value {
    let suggestions = match text {
        None => Vec::new(),
        Some(text) => vec![json!({ "text": text })],
    };
    json!({ PROMPT_SUGGESTIONS_META_KEY: { "suggestions": suggestions } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `src/tests/prompt-suggestions.test.ts | the follow-up prompt on the wire > drops a prediction that says nothing`
    #[test]
    fn drops_a_prediction_that_says_nothing() {
        assert_eq!(
            prompt_suggestion_text(Some("Run the tests")).as_deref(),
            Some("Run the tests")
        );
        assert_eq!(
            prompt_suggestion_text(Some("  Run the tests  ")).as_deref(),
            Some("Run the tests")
        );
        assert_eq!(prompt_suggestion_text(Some("   ")), None);
        assert_eq!(prompt_suggestion_text(Some("")), None);
        assert_eq!(prompt_suggestion_text(None), None);
    }

    /// `src/tests/prompt-suggestions.test.ts | the follow-up prompt on the wire > names no vendor and clears with an empty list`
    #[test]
    fn names_no_vendor_and_clears_with_an_empty_list() {
        assert_eq!(
            prompt_suggestions_meta(Some("Run the tests")),
            json!({ "promptSuggestions": { "suggestions": [{ "text": "Run the tests" }] } })
        );
        assert_eq!(
            prompt_suggestions_meta(None),
            json!({ "promptSuggestions": { "suggestions": [] } })
        );
    }

    /// The key is spelled exactly as upstream, with no vendor prefix.
    #[test]
    fn meta_key_is_vendor_neutral() {
        assert_eq!(PROMPT_SUGGESTIONS_META_KEY, "promptSuggestions");
        let meta = prompt_suggestions_meta(Some("next"));
        assert_eq!(meta.as_object().map(|map| map.len()), Some(1));
        assert!(meta.get(PROMPT_SUGGESTIONS_META_KEY).is_some());
    }

    /// Node goldens for `String.prototype.trim`: Unicode spaces trim, a
    /// zero-width space does not, and no input (empty, huge, `NaN`-like,
    /// non-ASCII) may panic.
    #[test]
    fn suggestion_text_stays_total_on_adversarial_inputs() {
        assert_eq!(
            prompt_suggestion_text(Some("\u{a0}Run\u{a0}")).as_deref(),
            Some("Run")
        );
        assert_eq!(prompt_suggestion_text(Some("\u{2003}")), None);
        assert_eq!(
            prompt_suggestion_text(Some("\u{200b}")).as_deref(),
            Some("\u{200b}")
        );
        assert_eq!(
            prompt_suggestion_text(Some("😀  run  😀")).as_deref(),
            Some("😀  run  😀")
        );
        assert_eq!(prompt_suggestion_text(Some("NaN")).as_deref(), Some("NaN"));

        let huge = "x".repeat(100_000);
        let kept = prompt_suggestion_text(Some(&huge));
        assert_eq!(kept.as_deref(), Some(huge.as_str()));
        assert_eq!(prompt_suggestion_text(Some(" \u{2003} \t\n")), None);
        assert_eq!(
            prompt_suggestions_meta(Some("😀")),
            json!({ "promptSuggestions": { "suggestions": [{ "text": "😀" }] } })
        );
    }
}
