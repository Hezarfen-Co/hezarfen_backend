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

/// The class's roster, newest first — by when the student was added. The
/// composite primary key is the tiebreaker (class, then user: the two halves
/// of the id this order used to sort), which makes the order total.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassMember>, i64), AppError> {
    PagedList::new(
        format!("{CLASS_MEMBER_TABLE} WHERE class = $1"),
        "ORDER BY added_at DESC, class DESC, app_user DESC",
    )
    .bind(class.uuid())
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
        format!("{CLASS_MEMBER_TABLE} WHERE app_user = $1"),
        "ORDER BY added_at DESC, class DESC, app_user DESC",
    )
    .bind(user.uuid())
    .run(limit, offset, db)
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::class_group::{ClassGroupId, ClassName};
    use crate::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};
    use crate::domain::user::UserId;

    /// A real `app_user` row for fixtures, by username — the username is
    /// unique, so every call for the same name shares one row, whichever call
    /// minted it. Creators, graders, and `added_by` are foreign keys now, so
    use sqlx::Row as _;

    /// fixture actors are rows, not fabricated ids.
    pub(crate) async fn fixture_user(db: &Database, username: &str) -> UserId {
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'student') ON CONFLICT DO NOTHING",
        )
        .bind(crate::domain::user::UserId::generate().uuid())
        .bind(username)
        .execute(db)
        .await
        .unwrap();
        let id: uuid::Uuid = sqlx::query("SELECT id FROM app_user WHERE username = $1")
            .bind(username)
            .fetch_one(db)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        UserId::from_key(&id.to_string())
    }

    pub(crate) async fn a_class(name: &str, db: &Database) -> ClassGroupId {
        let manager = fixture_user(db, "manager").await;
        crate::db::class_group::create(
            db,
            &manager,
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
        let manager = fixture_user(db, "manager").await;
        crate::db::course::create(
            db,
            &manager,
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

    /// A counter column on its row, re-read out of the store — never off a
    /// return value. The column name picks the table: every `*_count` these
    /// suites read lives on exactly one.
    pub(crate) async fn counter(field: &str, of: uuid::Uuid, db: &Database) -> i64 {
        let table = match field {
            "enrollment_count" => "course",
            "class_member_count" | "class_course_count" => "class_group",
            other => panic!("no table known for the counter {other}"),
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE({field}, 0) FROM {table} WHERE id = $1"
        )))
            .bind(of)
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    /// How many rows `table` holds.
    pub(crate) async fn rows(table: &str, db: &Database) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    /// Whether the course row is still there.
    pub(crate) async fn course_exists(course: &CourseId, db: &Database) -> bool {
        sqlx::query("SELECT 1 FROM course WHERE id = $1")
            .bind(course.uuid())
            .fetch_optional(db)
            .await
            .unwrap()
            .is_some()
    }

    /// Whether the enrollment pair still has its row.
    pub(crate) async fn enrollment_exists(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> bool {
        sqlx::query("SELECT 1 FROM enrollment WHERE course = $1 AND app_user = $2")
            .bind(course.uuid())
            .bind(user.uuid())
            .fetch_optional(db)
            .await
            .unwrap()
            .is_some()
    }

    /// Whether the class-course link is still attached.
    pub(crate) async fn link_exists(
        class: &ClassGroupId,
        course: &CourseId,
        db: &Database,
    ) -> bool {
        sqlx::query("SELECT 1 FROM class_course WHERE class = $1 AND course = $2")
            .bind(class.uuid())
            .bind(course.uuid())
            .fetch_optional(db)
            .await
            .unwrap()
            .is_some()
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
