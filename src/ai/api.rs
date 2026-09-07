//! Which REST paths an AI service may read.
//!
//! Deny-by-default: a request the bridge dispatches into the router has to
//! match one of [`AI_API_ALLOWLIST`]'s patterns exactly, segment for segment.
//! There is no prefix match and no wildcard tail — `/notes` never admits
//! `/notes/{id}/files/{file_id}` — so a new route family is unreachable to the
//! services until somebody adds it to the list on purpose.
//!
//! Pure path logic, deliberately independent of the rest of `ai`: it is the
//! access scope, and a scope that needed a connection to evaluate would be
//! harder to test than to bypass.

use crate::constant::AI_API_ALLOWLIST;

/// Whether an AI service may `GET` this path.
///
/// The path is the router path alone: a query string travels in its own frame
/// field, so a `?` here means the caller built the request wrong and the
/// request is refused rather than silently matched on its prefix.
pub fn path_allowed(path: &str) -> bool {
    route_template(path).is_some()
}

/// The allowlist pattern this path matched — the route *template*
/// (`/notes/{id}`), never the concrete path, which carries record ids. This is
/// what the bridge puts on a span or a metric label; see [`crate::telemetry`]
/// for why the concrete path may never leave the process.
pub fn route_template(path: &str) -> Option<&'static str> {
    if !path.starts_with('/') || path.contains('?') {
        return None;
    }
    AI_API_ALLOWLIST
        .iter()
        .copied()
        .find(|pattern| pattern_matches(pattern, path))
}

/// One `/`-separated pattern against one path. A literal segment must be
/// equal; a `{name}` placeholder takes exactly one non-empty segment.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let mut expected = pattern[1..].split('/');
    let mut actual = path[1..].split('/');
    loop {
        match (expected.next(), actual.next()) {
            (None, None) => return true,
            (Some(segment), Some(given)) => {
                let ok = if segment.starts_with('{') && segment.ends_with('}') {
                    !given.is_empty()
                } else {
                    segment == given
                };
                if !ok {
                    return false;
                }
            }
            // Different segment counts: a trailing slash or a deeper path is a
            // different route, never a longer match of this one.
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_path_matches_exactly() {
        assert!(path_allowed("/auth/me"));
        assert!(path_allowed("/notes"));
        assert!(!path_allowed("/auth/m"));
        assert!(!path_allowed("/AUTH/ME"));
    }

    #[test]
    fn placeholder_takes_one_segment() {
        assert!(path_allowed("/notes/note123"));
        assert!(path_allowed("/users/user456/profile"));
        // A placeholder is one segment, not a tail.
        assert!(!path_allowed("/notes/note123/files"));
        assert!(!path_allowed("/notes/note123/files/f1"));
    }

    #[test]
    fn course_notes_are_readable_but_their_blobs_are_not() {
        assert!(path_allowed("/course-notes"));
        assert!(path_allowed("/course-notes/note123"));
        // The listing of a note's files is JSON; the bytes behind one are not.
        assert!(path_allowed("/course-notes/note123/files"));
        assert!(!path_allowed("/course-notes/note123/files/f1"));
        assert!(!path_allowed("/course-notes/"));
    }

    #[test]
    fn trailing_slash_is_a_different_path() {
        assert!(!path_allowed("/notes/"));
        assert!(!path_allowed("/auth/me/"));
        assert!(!path_allowed("/notes/note123/"));
    }

    #[test]
    fn empty_segments_never_fill_a_placeholder() {
        assert!(!path_allowed("/notes//"));
        assert!(!path_allowed("//notes"));
        assert!(!path_allowed("/users//profile"));
    }

    #[test]
    fn the_template_is_the_pattern_not_the_path() {
        assert_eq!(route_template("/notes/note123"), Some("/notes/{id}"));
        assert_eq!(route_template("/auth/me"), Some("/auth/me"));
        assert_eq!(
            route_template("/users/user456/profile"),
            Some("/users/{id}/profile")
        );
        assert_eq!(route_template("/users"), None);
    }

    #[test]
    fn query_string_is_refused() {
        assert!(!path_allowed("/notes?limit=10"));
        assert!(!path_allowed("/notes/note123?x=1"));
    }

    #[test]
    fn unlisted_and_malformed_paths_are_denied() {
        assert!(!path_allowed(""));
        assert!(!path_allowed("notes"));
        assert!(!path_allowed("/"));
        // Real routes that are deliberately out of scope.
        assert!(!path_allowed("/users"));
        assert!(!path_allowed("/users/user456/avatar"));
        assert!(!path_allowed("/settings"));
        assert!(!path_allowed("/exams"));
    }
}
