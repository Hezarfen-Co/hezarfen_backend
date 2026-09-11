//! The `class_member` table: the roster reads. The membership writes are the
//! pump's, along the member axis ([`crate::db::class_pump`]); the
//! refusals-to-errors policy and the detach live in
//! [`crate::service::class_member`].

use crate::constant::CLASS_MEMBER_TABLE;
use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::ClassMember;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The class's roster, newest first — by when the student was added, not by
/// their account id, which is what the composite record id sorts on.
/// A row older than the column carries no stamp at all, and NONE sorts last
/// under DESC — which is the honest place for a row of unknown age.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassMember>, i64), AppError> {
    PagedList::new(
        format!("{CLASS_MEMBER_TABLE} WHERE class = $class"),
        "ORDER BY added_at DESC, id DESC",
    )
    .bind("class", class.record())
    .run(limit, offset, db)
    .await
}

/// The classes one student belongs to, newest membership first — the read
/// behind "which class section (şube) am I in". Same ordering story as
/// [`list_for_class`], along the other axis of the same index.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassMember>, i64), AppError> {
    PagedList::new(
        format!("{CLASS_MEMBER_TABLE} WHERE user = $usr"),
        "ORDER BY added_at DESC, id DESC",
    )
    .bind("usr", user.record())
    .run(limit, offset, db)
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::class_group::ClassName;
    use crate::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};
    use surrealdb::types::RecordId;

    pub(crate) async fn a_class(name: &str, db: &Database) -> ClassGroupId {
        crate::db::class_group::create(
            db,
            &UserId::from_key("manager"),
            ClassName::try_new(name).unwrap(),
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    pub(crate) async fn a_course(title: &str, capacity: Option<i64>, db: &Database) -> CourseId {
        crate::db::course::create(
            db,
            &UserId::from_key("manager"),
            CourseTitle::try_new(title).unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            capacity,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// A counter, re-read out of the store — never off a return value, which
    /// the in-memory engine forges wins on (see [`crate::db::cap`]).
    pub(crate) async fn counter(field: &str, of: RecordId, db: &Database) -> i64 {
        let mut result = db
            .query(format!("SELECT VALUE {field} ?? 0 FROM $of"))
            .bind(("of", of))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .first()
            .copied()
            .unwrap()
    }

    /// How many rows `sql` selects ids for.
    pub(crate) async fn rows(sql: &str, db: &Database) -> usize {
        let mut result = db.query(sql).await.unwrap().check().unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    /// Whether `id` names a live row.
    pub(crate) async fn exists(id: RecordId, db: &Database) -> bool {
        let mut result = db
            .query("SELECT VALUE id FROM $id")
            .bind(("id", id))
            .await
            .unwrap()
            .check()
            .unwrap();
        !result.take::<Vec<RecordId>>(0).unwrap().is_empty()
    }

    /// The class that wrote an enrollment, or `None` for a hand-placed row.
    pub(crate) async fn source_of(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Option<Option<ClassGroupId>> {
        crate::db::enrollment::read_for_user(db, course, user)
            .await
            .unwrap()
            .map(|row| row.get_source().cloned())
    }
}
