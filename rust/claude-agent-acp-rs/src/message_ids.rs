//! `Session.messageIdToUuid` (`acp-agent.ts:7712-7719`): the ACP messageId a
//! client sees -> the SDK transcript uuid that rewind/fork key on.
//!
//! The map is filled per consolidated `user`/`assistant` message (live) and by
//! the history replay (B-54). It is a pure store so the session actor can own
//! it without a lock across an await: the actor mutates it in place, and a
//! caller that needs it (the `session/fork` handler) asks the actor.

use std::collections::HashMap;

/// A per-session ACP messageId -> SDK uuid map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageIdMap {
    entries: HashMap<String, String>,
}

impl MessageIdMap {
    /// Record the uuid of a consolidated message under its ACP messageId.
    pub fn set(&mut self, message_id: &str, uuid: &str) {
        if !message_id.is_empty() && !uuid.is_empty() {
            self.entries
                .insert(message_id.to_string(), uuid.to_string());
        }
    }

    /// The SDK uuid of an ACP messageId, if this session saw the message.
    pub fn get(&self, message_id: &str) -> Option<&str> {
        self.entries.get(message_id).map(String::as_str)
    }

    /// How many ids the map holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Merge another map over this one (a replay backfills a live session's
    /// map; a later uuid wins).
    pub fn extend(&mut self, other: MessageIdMap) {
        self.entries.extend(other.entries);
    }

    /// Forget every mapping (a conversation reset mounts a fresh transcript).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_the_uuid_of_a_message_id() {
        let mut map = MessageIdMap::default();
        map.set("msg-1", "uuid-1");
        map.set("msg-1", "uuid-2");
        assert_eq!(map.get("msg-1"), Some("uuid-2"));
        assert_eq!(map.get("other"), None);
        assert_eq!(map.len(), 1);
        // Empty ids are not recorded (Node requires non-empty both).
        map.set("", "uuid-3");
        map.set("msg-4", "");
        assert_eq!(map.len(), 1);
        map.clear();
        assert!(map.is_empty());
    }
}
