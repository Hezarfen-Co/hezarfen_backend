//! Payload contract for the `chat.reply` capability.
//!
//! The transport is untouched: this is JSON that rides inside the existing
//! [`Request::payload`](crate::ai::protocol::Request::payload) and comes back
//! inside [`Response::Ok`](crate::ai::protocol::Response::Ok)`::payload`. One
//! request, one answer — the exchange stays unary, so a service answers with
//! the whole reply text rather than streaming tokens.
//!
//! The service is stateless about conversations. The backend owns the thread
//! and re-sends the relevant tail of it on every turn, which means a service
//! may restart, scale out, or be replaced mid-conversation without losing
//! context.

use serde::{Deserialize, Serialize};

/// The capability string routed to a chat service. Defined once, in
/// [`crate::constant`]; re-exported here so a reader of the payload contract
/// finds it next to the payloads.
pub use crate::constant::AI_CHAT_CAPABILITY;

/// Who said a turn. Serialises to `"user"` / `"assistant"` — the two names
/// every chat model API already uses, so a service can hand the history to its
/// model with no remapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
}

/// One previous message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
}

/// What the backend asks a chat service to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatRequestPayload {
    /// The user's new message. `history` does *not* contain it.
    pub message: String,
    /// The **school role** of the person asking — `"parent"`, `"student"`,
    /// `"teacher"`, `"manager"` or `"admin"`, the same lowercase strings the
    /// rest of the API uses ([`Role::as_str`](crate::domain::role::Role::as_str)).
    /// Deliberately not `role`: that name belongs to [`ChatTurn`], where it says
    /// who *said* a turn.
    ///
    /// Read live from the authenticated session on every request and never
    /// taken from the request body, so a service may answer on it: a student
    /// must not be handed an answer scoped for a manager. Scoping the answer is
    /// the service's job — the backend only forwards the role.
    pub asker_role: String,
    /// The last N turns of the conversation, **oldest first** — index 0 is the
    /// furthest back, the final element is the turn immediately before
    /// `message`. Same order a model's `messages` array expects, so a service
    /// appends `message` and sends it straight on.
    ///
    /// Absent or `[]` means a fresh conversation; a minimal service (or a
    /// first turn) can omit the key entirely.
    #[serde(default)]
    pub history: Vec<ChatTurn>,
}

/// The service's answer, inside `Response::Ok { payload }`.
///
/// Handled failures do *not* come back here — they use the existing
/// [`Response::Err`](crate::ai::protocol::Response::Err) `{ code, message }`,
/// so there is exactly one failure shape on the bridge for every capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatReplyPayload {
    /// The complete reply text. Not chunked, not partial.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::role::Role;
    use serde_json::json;

    #[test]
    fn payloads_round_trip() {
        let request = ChatRequestPayload {
            message: "and the second?".into(),
            asker_role: Role::Student.as_str().to_string(),
            history: vec![
                ChatTurn {
                    role: ChatRole::User,
                    content: "what is the first law?".into(),
                },
                ChatTurn {
                    role: ChatRole::Assistant,
                    content: "inertia".into(),
                },
            ],
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<ChatRequestPayload>(encoded).unwrap(),
            request
        );

        let reply = ChatReplyPayload {
            text: "F = ma".into(),
        };
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            serde_json::from_value::<ChatReplyPayload>(encoded).unwrap(),
            reply
        );
    }

    #[test]
    fn chat_payload_keys_are_the_documented_wire_names() {
        // Services in other languages match on these literals — a rename here
        // silently breaks every one of them, so pin the encoding.
        let request = serde_json::to_value(ChatRequestPayload {
            message: "hi".into(),
            asker_role: Role::Teacher.as_str().to_string(),
            history: vec![
                ChatTurn {
                    role: ChatRole::User,
                    content: "a".into(),
                },
                ChatTurn {
                    role: ChatRole::Assistant,
                    content: "b".into(),
                },
            ],
        })
        .unwrap();
        assert_eq!(
            request,
            json!({
                "message": "hi",
                "asker_role": "teacher",
                "history": [
                    { "role": "user", "content": "a" },
                    { "role": "assistant", "content": "b" },
                ],
            })
        );
        // Every role reaches the wire as its documented lowercase slug — the
        // one `Role::as_str` already publishes, not a second spelling.
        for role in crate::domain::role::ROLES {
            let encoded = serde_json::to_value(ChatRequestPayload {
                message: "hi".into(),
                asker_role: role.as_str().to_string(),
                history: Vec::new(),
            })
            .unwrap();
            assert_eq!(encoded["asker_role"], role.as_str());
        }
        assert_eq!(
            serde_json::to_value(ChatReplyPayload { text: "ok".into() }).unwrap(),
            json!({ "text": "ok" })
        );
        assert_eq!(AI_CHAT_CAPABILITY, "chat.reply");
    }

    #[test]
    fn history_is_optional_on_the_wire() {
        // The smallest legal request: a first turn, from a service author who
        // never sends an empty array.
        let bare: ChatRequestPayload =
            serde_json::from_value(json!({ "message": "hi", "asker_role": "student" })).unwrap();
        assert!(bare.history.is_empty());
        let empty: ChatRequestPayload = serde_json::from_value(
            json!({ "message": "hi", "asker_role": "student", "history": [] }),
        )
        .unwrap();
        assert_eq!(bare, empty);
    }

    #[test]
    fn history_keeps_its_oldest_first_order() {
        // Round-tripping must not reorder: a service reading index 0 as the
        // oldest turn depends on it.
        let raw = json!({
            "message": "third",
            "asker_role": "student",
            "history": [
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "second" },
            ],
        });
        let payload: ChatRequestPayload = serde_json::from_value(raw).unwrap();
        assert_eq!(payload.history[0].content, "first");
        assert_eq!(payload.history[0].role, ChatRole::User);
        assert_eq!(payload.history[1].content, "second");
        assert_eq!(payload.history[1].role, ChatRole::Assistant);
    }
}
