use crate::domain::user::UserId;

/// The (parent, student) pair — the table's natural composite primary key.
/// The same pair always maps to the same row, so linking is a single atomic
/// upsert with no find-then-insert race and one-row-per-pair by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentLinkId {
    parent: UserId,
    student: UserId,
}

impl ParentLinkId {
    pub fn composite(parent: &UserId, student: &UserId) -> Self {
        Self {
            parent: *parent,
            student: *student,
        }
    }

    /// Parse the `{parent}_{student}` wire form. A key that parses as no pair
    /// reads as the nil pair, which matches no row — exactly the 404 a
    /// dangling composite key produced under the old store, without turning a
    /// typo into a panic. UUID strings carry only `-`, so `_` is an
    /// unambiguous joiner.
    pub fn from_key(key: &str) -> Self {
        let (parent, student) = key.rsplit_once('_').unwrap_or(("", ""));
        Self {
            parent: UserId::from_key(parent),
            student: UserId::from_key(student),
        }
    }

    /// The `{parent}_{student}` wire form.
    pub fn key(&self) -> String {
        format!("{}_{}", self.parent.key(), self.student.key())
    }

    pub fn parent(&self) -> UserId {
        self.parent
    }

    pub fn student(&self) -> UserId {
        self.student
    }
}

/// A parent account's tie to one student it may observe. Almost entirely a
/// read grant: it gates the parent's access to the student's reports (marks,
/// attendance, pomodoro). The single write it authorizes is the food program —
/// a parent books and cancels a linked child's meals, since paying for lunch
/// is a parent's job and a small child cannot do it themselves.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ParentLink {
    pub(crate) parent: UserId,
    pub(crate) student: UserId,
    pub(crate) linked_by: UserId,
}

impl ParentLink {
    pub fn get_parent(&self) -> &UserId {
        &self.parent
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_linked_by(&self) -> &UserId {
        &self.linked_by
    }

    // Both role-change sweeps (the observed student leaving `student`, the
    // observing parent leaving `parent`) live in
    // [`crate::service::user::set_role`]: they commit with the role write
    // itself, so there is no window where a link outlives the role that
    // justified it.
}
