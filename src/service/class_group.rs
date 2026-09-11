//! Class workflows: the archived-term guard every class write re-checks, and
//! the thin route-facing wrappers the handlers call. The queries live in
//! [`crate::db::class_group`].

use crate::database::Database;
use crate::db::class_group;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId, ClassName};
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Refuse the write when this class's term is archived — a pre-flight guard,
/// accepted race (see README concurrency model): a term archived after this
/// read still lets the write through.
pub async fn require_open(db: &Database, class: &ClassGroup) -> Result<(), AppError> {
    match class.get_term() {
        None => Ok(()),
        Some(term) => crate::service::term::require_open(db, term).await,
    }
}

/// Create a class, claiming the term reference in the same transaction —
/// see [`class_group::create`].
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: ClassName,
    grade: Option<ClassGrade>,
    term: Option<TermId>,
    teacher: Option<UserId>,
) -> Result<ClassGroup, AppError> {
    class_group::create(db, creator, name, grade, term, teacher).await
}

/// The row, for callers that only inspect it — the web layer's
/// `class_or_404` reads through here.
pub async fn read(db: &Database, id: &ClassGroupId) -> Result<Option<ClassGroup>, AppError> {
    class_group::read(db, id).await
}

/// Every class, newest first — or one grade's, when `grade` narrows it: the
/// read behind the class index.
pub async fn list_all(
    db: &Database,
    grade: Option<Option<ClassGrade>>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassGroup>, i64), AppError> {
    class_group::list_all(db, grade, limit, offset).await
}

/// The classes `ids` names, in no particular order — the join behind the two
/// membership reads.
pub async fn list_by_ids(db: &Database, ids: &[ClassGroupId]) -> Result<Vec<ClassGroup>, AppError> {
    class_group::list_by_ids(db, ids).await
}

/// Write only the fields the PATCH carried; a term move claims the new term
/// and releases the old one inside the very transaction that moves the link.
pub async fn update(
    db: &Database,
    class: ClassGroup,
    name: Option<ClassName>,
    grade: Option<Option<ClassGrade>>,
    term: Option<Option<TermId>>,
    teacher: Option<Option<UserId>>,
) -> Result<ClassGroup, AppError> {
    class_group::update(db, class, name, grade, term, teacher).await
}

/// Delete the class and give its term reference back; `false` = refused,
/// nothing was written (the class still holds students or courses).
pub async fn delete(db: &Database, class: ClassGroup) -> Result<bool, AppError> {
    class_group::delete(db, class).await
}

/// Strip `user` from every class they were the homeroom teacher of — the
/// sweep for a user demoted below `teacher`.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    class_group::unassign_everywhere(db, user).await
}
