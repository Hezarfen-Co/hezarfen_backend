use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::database::Database;
use crate::rate_limit::RateLimitConfig;

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    /// Directory holding uploaded note-file blobs, one file per
    /// `note_file` row, named by the row's key
    /// (from [`crate::config::Config::files_path`]).
    pub files_path: PathBuf,
    /// Whether the session cookie carries the `Secure` attribute
    /// (from [`crate::config::Config::cookie_secure`]).
    pub cookie_secure: bool,
    /// Per-IP request limits (from [`crate::config::Config::rate_limit`]).
    /// Read once by [`crate::build_router`] when the limiters are built.
    pub rate_limit: RateLimitConfig,
    /// Who is inside which exam room right now (see [`ExamPresence`]).
    pub exam_presence: ExamPresence,
}

/// How many exam-room sockets each attempt has open right now, keyed by the
/// attempt's record key. The room counts itself in on entry and out on exit,
/// and only the last socket out stamps the walk-out (`left_at`) — a student
/// closing one of two tabs never counts as having left the exam.
///
/// The inner mutex only guards the map (never held across an await — keep it
/// that way). Each count transition and its matching `left_at` DB write are
/// serialized as one critical section by the exam room's `PRESENCE_LOCK`
/// (see `web::exam_ws`), so "clear on join" and "stamp on last-out" stay
/// atomic with the counts they depend on.
#[derive(Clone, Default)]
pub struct ExamPresence(Arc<Mutex<HashMap<String, usize>>>);

impl ExamPresence {
    /// Count one socket into `attempt`'s room.
    pub fn enter(&self, attempt: &str) {
        let mut rooms = self.0.lock().expect("exam presence lock");
        *rooms.entry(attempt.to_string()).or_insert(0) += 1;
    }

    /// Count one socket out of `attempt`'s room; `true` when it was the last
    /// one — the student has actually left.
    pub fn leave(&self, attempt: &str) -> bool {
        let mut rooms = self.0.lock().expect("exam presence lock");
        match rooms.get_mut(attempt) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                rooms.remove(attempt);
                true
            }
            // Unbalanced (an enter was lost) — treat the close as a real exit
            // rather than leaving the walk-out unrecorded forever.
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_the_last_socket_out_is_a_real_exit() {
        let presence = ExamPresence::default();
        presence.enter("attempt-a");
        presence.enter("attempt-a");
        assert!(
            !presence.leave("attempt-a"),
            "one tab of two is not an exit"
        );
        assert!(presence.leave("attempt-a"), "the last tab out is");
        // Rooms are independent, and a fresh key starts over.
        presence.enter("attempt-b");
        presence.enter("attempt-a");
        assert!(presence.leave("attempt-b"));
        assert!(presence.leave("attempt-a"));
        // Unbalanced leaves degrade to "really left", never to a stuck room.
        assert!(presence.leave("attempt-a"));
    }
}
