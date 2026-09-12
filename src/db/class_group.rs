//! The `class_group` table: the row, its term reference claimed through the
//! [`crate::db::cap`] shapes in the same statement as the link, and the 0/0
//! delete guard. The archived-term guard and the route-facing wrappers live in
//! [`crate::service::class_group`].

use crate::constant::{CLASS_GROUP_TABLE, TERM_CLASS_COUNT_FIELD};
use crate::database::{Database, tx_with_retry};
use crate::db::field_update::{FieldUpdate, Refcount};
use crate::db::page::PagedList;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId, ClassName};
use crate::domain::term::{self, TermId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Create the class, claiming a reference on the term it links (if any) in
/// the *same statement* as the row, exactly as
/// [`crate::db::course::create`] does: the claim is a
/// conditional write on the term row, so it fails when the term is already
/// gone, it makes the term undeletable the instant this link exists, and — the
/// claim and the insert being one CTE — no crash and no refused claim can
/// leave either half without the other.
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: ClassName,
    grade: Option<ClassGrade>,
    term: Option<TermId>,
    teacher: Option<UserId>,
) -> Result<ClassGroup, AppError> {
    let id = ClassGroupId::generate();
    match &term {
        Some(term) => {
            // The claim and the insert are one statement: atomic without an
            // explicit transaction, exactly like every other
            // single-statement guard.
            let written = sqlx::query_as!(
                ClassGroup,
                r#"WITH seat AS (
                       UPDATE term SET class_count = class_count + 1
                        WHERE id = $1
                        RETURNING 1)
                   INSERT INTO class_group (id, creator, name, grade, term, teacher)
                   SELECT $2, $3, $4, $5, $1, $6 WHERE EXISTS (SELECT 1 FROM seat)
                   RETURNING id AS "id: ClassGroupId", creator AS "creator: UserId",
                     name AS "name: ClassName", grade AS "grade: ClassGrade",
                     term AS "term: TermId", teacher AS "teacher: UserId""#,
                term.uuid(),
                id.uuid(),
                creator.uuid(),
                name.as_str(),
                grade.as_ref().map(ClassGrade::as_str),
                teacher.as_ref().map(UserId::uuid)
            )
            .fetch_optional(db)
            .await;
            match written {
                Ok(Some(created)) => Ok(created),
                // Uncapped, so a zero-row claim can only mean the conditional
                // write matched no term row at all — the claim doubles as the
                // existence check.
                Ok(None) => Err(term::gone_error()),
                // The term row is locked by the claim's own UPDATE, so this
                // is unreachable; a defensible answer beats a 500.
                Err(e) if crate::database::foreign_key_violation(&e) => Err(term::gone_error()),
                // Unreachable: the id was minted one line above.
                Err(e) if crate::database::unique_violation(&e).is_some() => {
                    Err(AppError::Internal("failed to create class".into()))
                }
                Err(e) => Err(e.into()),
            }
        }
        None => {
            let created = sqlx::query_as!(
                ClassGroup,
                r#"INSERT INTO class_group (id, creator, name, grade, term, teacher)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   RETURNING id AS "id: ClassGroupId", creator AS "creator: UserId",
                     name AS "name: ClassName", grade AS "grade: ClassGrade",
                     term AS "term: TermId", teacher AS "teacher: UserId""#,
                id.uuid(),
                creator.uuid(),
                name.as_str(),
                grade.as_ref().map(ClassGrade::as_str),
                term.as_ref().map(TermId::uuid),
                teacher.as_ref().map(UserId::uuid)
            )
            .fetch_optional(db)
            .await?;
            // Unreachable: the id is a v7 uuid this call just generated.
            created.ok_or_else(|| AppError::Internal("failed to create class".into()))
        }
    }
}

pub async fn read(db: &Database, id: &ClassGroupId) -> Result<Option<ClassGroup>, AppError> {
    let class = sqlx::query_as!(
        ClassGroup,
        r#"SELECT id AS "id: ClassGroupId", creator AS "creator: UserId",
                  name AS "name: ClassName", grade AS "grade: ClassGrade",
                  term AS "term: TermId", teacher AS "teacher: UserId"
           FROM class_group WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(class)
}

/// Every class, newest first — or one grade's, when `grade` narrows it:
/// `Some(Some(label))` is the sections carrying that exact label (no trim,
/// no case folding, matching [`list_for_grade`] and the
/// blueprint keyed by that very string), `Some(None)` the sections carrying
/// no grade at all, `None` the whole list.
///
/// The narrowing is the `WHERE`, so the `total` [`PagedList`] counts is the
/// filtered set and a client can page through it.
pub async fn list_all(
    db: &Database,
    grade: Option<Option<ClassGrade>>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassGroup>, i64), AppError> {
    let list = match grade {
        None => PagedList::new(CLASS_GROUP_TABLE, "ORDER BY id DESC"),
        Some(None) => PagedList::new(
            format!("{CLASS_GROUP_TABLE} WHERE grade IS NULL"),
            "ORDER BY id DESC",
        ),
        Some(Some(grade)) => PagedList::new(
            format!("{CLASS_GROUP_TABLE} WHERE grade = $1"),
            "ORDER BY id DESC",
        )
        .bind(grade.as_str().to_string()),
    };
    list.run(limit, offset, db).await
}

/// Write only the fields the PATCH carried — `None` means the request
/// omitted it, so the column is left alone rather than re-stated from the
/// snapshot this struct was read into. `grade`, `term` and `teacher` are
/// nullable, so they take the outer/inner `Option<Option<_>>`: `None` =
/// omitted (keep), `Some(None)` = clear.
///
/// A term move claims the new term and releases the old one inside the very
/// transaction that moves the link, exactly as in
/// [`crate::db::course::update`]: both counters and the link
/// commit together, so no crash can strand a count on a term nothing links.
pub async fn update(
    db: &Database,
    class: ClassGroup,
    name: Option<ClassName>,
    grade: Option<Option<ClassGrade>>,
    term: Option<Option<TermId>>,
    teacher: Option<Option<UserId>>,
) -> Result<ClassGroup, AppError> {
    let (claim, release) = term::ref_move(class.term.as_ref(), &term);
    let expected = class.term.as_ref().map(TermId::uuid);
    FieldUpdate::new(CLASS_GROUP_TABLE, class.id.uuid())
        .set("name", name.map(|name| name.as_str().to_string()))
        .set(
            "grade",
            grade.map(|grade| grade.map(|grade| grade.as_str().to_string())),
        )
        .set("term", term.map(|term| term.map(|term| term.uuid())))
        // Not refcounted: a homeroom assignment is a label, so it rides the
        // plain `set` path and never arms the term CAS.
        .set(
            "teacher",
            teacher.map(|teacher| teacher.map(|teacher| teacher.uuid())),
        )
        .refcount(Refcount {
            counter_table: "term",
            counter_field: TERM_CLASS_COUNT_FIELD,
            link: "term",
            expected,
            claim: claim.map(|term| term.uuid()),
            release: release.map(|term| term.uuid()),
            refused: term::gone_error(),
        })
        .run::<ClassGroup>(db)
        .await
}

/// The classes `ids` name, in no particular order — the join behind
/// "which class section is this student in", where the ids come from
/// `class_member` rows already paged. Ids that name no row are simply
/// absent.
pub async fn list_by_ids(db: &Database, ids: &[ClassGroupId]) -> Result<Vec<ClassGroup>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = ids.iter().map(ClassGroupId::uuid).collect();
    let classes = sqlx::query_as!(
        ClassGroup,
        r#"SELECT id AS "id: ClassGroupId", creator AS "creator: UserId",
                  name AS "name: ClassName", grade AS "grade: ClassGrade",
                  term AS "term: TermId", teacher AS "teacher: UserId"
           FROM class_group WHERE id = ANY($1)"#,
        &ids
    )
    .fetch_all(db)
    .await?;
    Ok(classes)
}

/// Every class section at one grade label, in no particular order — what a
/// grade blueprint pumps. Unpaged on purpose: the caller is reconciling all
/// of them, and a page would silently stock only the first window.
pub async fn list_for_grade(
    db: &Database,
    grade: &ClassGrade,
) -> Result<Vec<ClassGroup>, AppError> {
    let classes = sqlx::query_as!(
        ClassGroup,
        r#"SELECT id AS "id: ClassGroupId", creator AS "creator: UserId",
                  name AS "name: ClassName", grade AS "grade: ClassGrade",
                  term AS "term: TermId", teacher AS "teacher: UserId"
           FROM class_group WHERE grade = $1"#,
        grade.as_str()
    )
    .fetch_all(db)
    .await?;
    Ok(classes)
}

/// Clear the homeroom teacher everywhere `user` held one — the
/// sweep for a user demoted below `teacher`, who may no longer hold one.
/// The mirror of [`crate::db::course::unassign_everywhere`];
/// nothing is counted on this column, so there is no reference to give back.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    sqlx::query!(
        r#"UPDATE class_group SET teacher = NULL WHERE teacher = $1"#,
        user.uuid()
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Delete the class and give its term reference back. Nothing else cascades:
/// a class that still holds students or courses is refused outright, because
/// dropping it silently would leave the enrollments it pumped behind with
/// nothing left to sweep them. An event aimed at the class keeps standing
/// with `audience_class` cleared — the course twin's documented outcome
/// (`db::course::delete`): the roster resolves live, so it simply reads empty.
///
/// `false` = refused, nothing was written. Both counts are read off the
/// class's own row, so the check and the delete are one conditional write on
/// one record — a member or attach racing this either claims first (and the
/// delete is refused) or finds the row gone (and is refused itself). The
/// `Err(NotFound)` keeps the answer a concurrent *delete* used to get.
pub async fn delete(db: &Database, class: ClassGroup) -> Result<bool, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // Events aimed at this class keep standing, audience cleared — the
        // hard FK on `event.audience_class` would otherwise refuse the delete
        // outright, and a dangling audience resolves to an empty roster.
        sqlx::query!(
            r#"UPDATE event SET audience_class = NULL WHERE audience_class = $1"#,
            class.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        let gone = sqlx::query!(
            r#"DELETE FROM class_group
               WHERE id = $1
                 AND class_member_count = 0 AND class_course_count = 0
               RETURNING term AS "term: uuid::Uuid""#,
            class.id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = gone else {
            // Still linked or already gone: the guard cannot tell those
            // apart, and only the refusal path pays for the extra read that
            // can.
            let standing = sqlx::query_scalar!(
                r#"SELECT 1 AS "one" FROM class_group WHERE id = $1"#,
                class.id.uuid()
            )
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            return if standing {
                Ok(false)
            } else {
                Err(AppError::NotFound)
            };
        };
        if let Some(term) = row.term {
            sqlx::query!(
                r#"UPDATE term SET class_count = GREATEST(class_count - 1, 0)
                   WHERE id = $1"#,
                term
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(true)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row as _;

    use crate::domain::term::{Term, TermName};

    /// A real `app_user` row: creator, teacher, and attached-by are foreign
    /// keys now, so every fixture participant is a row, not a fabricated id.
    async fn a_named_user(db: &Database, username: &str) -> UserId {
        // The username is unique: a second call for the same name must adopt
        // the existing row, not collide with it.
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'teacher') ON CONFLICT DO NOTHING",
        )
        .bind(UserId::generate().uuid())
        .bind(username)
        .execute(db)
        .await
        .unwrap();
        let id: uuid::Uuid =
            sqlx::query("SELECT id FROM app_user WHERE username = $1")
                .bind(username)
                .fetch_one(db)
                .await
                .unwrap()
                .try_get(0)
                .unwrap();
        UserId::from_key(&id.to_string())
    }

    async fn class_on(term: Option<TermId>, db: &Database) -> ClassGroup {
        let manager = a_named_user(db, "manager").await;
        create(
            db,
            &manager,
            ClassName::try_new("9-A").unwrap(),
            None,
            term,
            None,
        )
        .await
        .unwrap()
    }

    /// A class with a homeroom teacher, no term.
    async fn class_of(teacher: Option<UserId>, db: &Database) -> ClassGroup {
        let manager = a_named_user(db, "manager").await;
        create(
            db,
            &manager,
            ClassName::try_new("9-A").unwrap(),
            None,
            None,
            teacher,
        )
        .await
        .unwrap()
    }

    /// The stored `teacher` column, re-read.
    async fn teacher_of(class: &ClassGroupId, db: &Database) -> Option<UserId> {
        read(db, class)
            .await
            .unwrap()
            .unwrap()
            .get_teacher()
            .cloned()
    }

    async fn a_term(db: &Database) -> Term {
        let at = crate::domain::timestamp::Timestamp::from_millis;
        crate::db::term::create(db, TermName::try_new("2026").unwrap(), at(100), at(200))
            .await
            .unwrap()
    }

    /// The single integer `sql` selects — the stored counter, re-read, never
    /// off a return value. The SQL is built from literals in this module, so
    /// the audit wrapper is a formality.
    async fn one_i64(db: &Database, sql: String) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    /// The stored `class_count` on one term, absent counting as zero.
    async fn count_on(term: &TermId, db: &Database) -> i64 {
        one_i64(
            db,
            format!("SELECT COALESCE(class_count, 0) FROM term WHERE id = '{}'", term.uuid()),
        )
        .await
    }

    /// How many rows `table` holds.
    async fn row_count(db: &Database, table: &str) -> i64 {
        one_i64(db, format!("SELECT count(*) FROM {table}")).await
    }

    /// The homeroom teacher survives a create, is set and cleared by a PATCH,
    /// and — the part a `.content(self)` save would break — is left alone by a
    /// PATCH of another field. A row written before the column exists carries
    /// no key at all, which must read back as `None`, not fail the decode.
    #[tokio::test]
    async fn the_homeroom_teacher_is_stored_set_cleared_and_left_alone() {
        let (db, _leases) = crate::database::init_test_db().await;
        let ada = a_named_user(&db, "ada").await;
        let boole = a_named_user(&db, "boole").await;

        let bare = class_of(None, &db).await;
        assert_eq!(teacher_of(bare.get_id(), &db).await, None);
        let held = class_of(Some(ada), &db).await;
        assert_eq!(teacher_of(held.get_id(), &db).await, Some(ada));

        // A name-only PATCH must not re-state the teacher out of its snapshot.
        let renamed = update(
            &db,
            held,
            Some(ClassName::try_new("9-B").unwrap()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(renamed.get_name().as_str(), "9-B");
        assert_eq!(teacher_of(renamed.get_id(), &db).await, Some(ada));

        let moved = update(&db, renamed, None, None, None, Some(Some(boole)))
            .await
            .unwrap();
        assert_eq!(teacher_of(moved.get_id(), &db).await, Some(boole));

        let cleared = update(&db, moved, None, None, None, Some(None))
            .await
            .unwrap();
        assert_eq!(teacher_of(cleared.get_id(), &db).await, None);

        // …and the mirror: a teacher-only PATCH leaves the name alone.
        let again = update(
            &db,
            cleared,
            None,
            None,
            None,
            Some(Some(a_named_user(&db, "ada").await)),
        )
        .await
        .unwrap();
        assert_eq!(again.get_name().as_str(), "9-B");

        // A row without a homeroom: the column reads back as NULL, not a
        // decode failure.
        sqlx::query("UPDATE class_group SET teacher = NULL WHERE id = $1")
            .bind(bare.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(teacher_of(bare.get_id(), &db).await, None);
    }

    /// The demotion sweep: a user who drops below `teacher` is cleared from
    /// *every* class they homeroomed, and nobody else's is touched.
    #[tokio::test]
    async fn unassign_everywhere_clears_only_that_users_classes() {
        let (db, _leases) = crate::database::init_test_db().await;
        let ada = a_named_user(&db, "ada").await;
        let boole = a_named_user(&db, "boole").await;
        let first = class_of(Some(ada), &db).await;
        let second = class_of(Some(ada), &db).await;
        let other = class_of(Some(boole), &db).await;
        let none = class_of(None, &db).await;

        unassign_everywhere(&db, &ada).await.unwrap();
        for class in [&first, &second] {
            assert_eq!(
                teacher_of(class.get_id(), &db).await,
                None,
                "every class the demoted user homeroomed must be cleared"
            );
        }
        assert_eq!(
            teacher_of(other.get_id(), &db).await,
            Some(boole),
            "…and nobody else's"
        );
        assert_eq!(teacher_of(none.get_id(), &db).await, None);
    }

    /// The bite test for the class half of the term delete guard: a term is
    /// undeletable while a *class* links it, on its own column, and every way
    /// that link can end gives the reference back. Claiming into `course_count`
    /// instead would pass the first assert and fail the roundtrip through boot.
    #[tokio::test]
    async fn a_term_is_deletable_only_once_no_class_links_it() {
        let (db, _leases) = crate::database::init_test_db().await;
        let term = a_term(&db).await;

        let linked = class_on(Some(*term.get_id()), &db).await;
        let patched = class_on(Some(*term.get_id()), &db).await;
        assert_eq!(
            one_i64(&db, "SELECT COALESCE(class_count, 0) FROM term".to_string()).await,
            2,
            "classes must count on class_count, not course_count"
        );
        assert_eq!(
            one_i64(&db, "SELECT COALESCE(course_count, 0) FROM term".to_string()).await,
            0,
            "the courses' counter is seeded from course rows and must stay untouched"
        );
        assert!(
            !crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "two linked classes must refuse the delete"
        );

        update(&db, patched, None, None, Some(None), None)
            .await
            .unwrap();
        assert!(
            !crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "one link is still one link"
        );

        assert!(delete(&db, linked).await.unwrap());
        assert!(
            crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "the last link gone, the term may go"
        );
    }

    /// The bite test for the class delete guard, both arms: either count above
    /// zero refuses, having written nothing — the term reference least of all,
    /// which a refusal that released it would strand.
    #[tokio::test]
    async fn a_class_with_members_or_courses_refuses_to_delete() {
        for field in ["class_member_count", "class_course_count"] {
            let (db, _leases) = crate::database::init_test_db().await;
            let term = a_term(&db).await;
            let class = class_on(Some(*term.get_id()), &db).await;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE class_group SET {field} = 1 WHERE id = $1"
            )))
                .bind(class.get_id().uuid())
                .execute(&db)
                .await
                .unwrap();

            assert!(
                !delete(&db, class.clone()).await.unwrap(),
                "{field} above zero must refuse the delete"
            );
            assert!(
                read(&db, class.get_id()).await.unwrap().is_some(),
                "a refused delete may write nothing"
            );
            assert_eq!(
                one_i64(&db, "SELECT COALESCE(class_count, 0) FROM term".to_string()).await,
                1,
                "…the term reference least of all"
            );

            // Back to zero, and the same class deletes and releases the term.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE class_group SET {field} = 0 WHERE id = $1"
            )))
                .bind(class.get_id().uuid())
                .execute(&db)
                .await
                .unwrap();
            assert!(delete(&db, class.clone()).await.unwrap());
            assert_eq!(
                one_i64(&db, "SELECT COALESCE(class_count, 0) FROM term".to_string()).await,
                0
            );
            let again = delete(&db, class).await;
            assert!(
                matches!(again, Err(AppError::NotFound)),
                "a second delete is a 404, not a refusal: {again:?}"
            );
        }
    }

    /// The class half of the invariant on the create path: a refused create
    /// leaves neither the row nor a count stranded on a term (which the term's
    /// delete guard reads, so a stray one would make it undeletable forever).
    #[tokio::test]
    async fn a_refused_create_writes_neither_row_nor_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let term = a_term(&db).await;
        let id = *term.get_id();
        assert!(crate::db::term::delete(&db, term).await.unwrap());

        let error = create(
            &db,
            &a_named_user(&db, "manager").await,
            ClassName::try_new("9-A").unwrap(),
            None,
            Some(id),
            None,
        )
        .await
        .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
        assert_eq!(
            row_count(&db, "class_group").await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            row_count(&db, "term").await,
            0,
            "…and least of all a count on a term it just brought back"
        );
    }

    /// The class mirror of
    /// [`crate::db::course`]'s move tests: a term move carries both
    /// counters with the link, and a move to a term that is gone rolls the
    /// release back with the abort (the transaction releases before it claims,
    /// so the old count would be 0 if the abort did not undo it).
    #[tokio::test]
    async fn a_class_term_move_moves_both_counts_or_neither() {
        let (db, _leases) = crate::database::init_test_db().await;
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let from = a_term(&db).await;
        let to = crate::db::term::create(&db, TermName::try_new("2027").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let dead =
            crate::db::term::create(&db, TermName::try_new("2028").unwrap(), at(100), at(200))
                .await
                .unwrap();
        let dead_id = *dead.get_id();
        assert!(crate::db::term::delete(&db, dead).await.unwrap());
        let class = class_on(Some(*from.get_id()), &db).await;

        let error = update(
            &db,
            class.clone(),
            Some(ClassName::try_new("9-B").unwrap()),
            None,
            Some(Some(dead_id)),
            None,
        )
        .await
        .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
        let stored = read(&db, class.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(from.get_id()), "the link stays put");
        assert_eq!(stored.get_name().as_str(), "9-A", "…and so does the row");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release must roll back with the abort"
        );

        let moved = update(
            &db,
            class,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(moved.get_term(), Some(to.get_id()));
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the old term is free"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
    }

    /// The class twin of
    /// [`crate::domain::course`]'s `a_stale_mover_is_refused_and_claims_nothing`
    /// and `a_stale_re_stater_is_refused_and_reverts_nothing`, in one: both
    /// PATCHes compute their counter move from the row as *they* read it, so a
    /// second one running on the pre-move struct would claim a second seat for
    /// one link (the mover) or drag the link back and strand the winner's claim
    /// (the re-stater, which shifts no counter at all and so is only ever
    /// stopped by a guard armed off the *carried column*). Both must be refused
    /// with the counts reading as if they never ran.
    #[tokio::test]
    async fn a_stale_class_term_write_is_refused_and_moves_no_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let from = a_term(&db).await;
        let to = crate::db::term::create(&db, TermName::try_new("2027").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let other =
            crate::db::term::create(&db, TermName::try_new("2028").unwrap(), at(100), at(200))
                .await
                .unwrap();
        let class = class_on(Some(*from.get_id()), &db).await;
        let stale = class.clone();
        update(
            &db,
            class,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();

        // The mover: its snapshot says `from`, so it would release `from` and
        // claim `other` on top of the winner's claim on `to`.
        let error = update(
            &db,
            stale.clone(),
            None,
            None,
            Some(Some(*other.get_id())),
            None,
        )
        .await
        .expect_err("a mover that read a link it no longer holds must be refused");
        assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
        // The re-stater: shifts no counter, so only the CAS can stop it.
        let error = update(
            &db,
            stale.clone(),
            None,
            None,
            Some(Some(*from.get_id())),
            None,
        )
        .await
        .expect_err("re-stating a link someone else moved must be refused");
        assert!(matches!(error, AppError::Conflict(_)), "{error:?}");

        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(to.get_id()), "the winner's link");
        assert_eq!(count_on(from.get_id(), &db).await, 0, "released once");
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once");
        assert_eq!(count_on(other.get_id(), &db).await, 0, "never claimed");

        // A genuine no-op re-state still lands and still moves nothing.
        let same = update(
            &db,
            stored,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .expect("re-stating the link the row really holds is not a conflict");
        assert_eq!(same.get_term(), Some(to.get_id()));
        assert_eq!(count_on(to.get_id(), &db).await, 1, "still one seat");
    }

    /// [`crate::db::course::delete`]'s class sweep: deleting a course
    /// takes its `class_course` attachments with it and gives each class its
    /// count back, or the classes would be undeletable forever over rows that
    /// point at nothing.
    #[tokio::test]
    async fn deleting_a_course_sweeps_its_class_attachments() {
        use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};

        let (db, _leases) = crate::database::init_test_db().await;
        let manager = a_named_user(&db, "manager").await;
        let course = crate::db::course::create(
            &db,
            &manager,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            None,
        )
        .await
        .unwrap();
        let class = class_on(None, &db).await;
        sqlx::query("INSERT INTO class_course (class, course, attached_by) VALUES ($1, $2, $3)")
            .bind(class.get_id().uuid())
            .bind(course.get_id().uuid())
            .bind(manager.uuid())
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE class_group SET class_course_count = 1 WHERE id = $1")
            .bind(class.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();

        assert!(crate::db::course::delete(&db, course).await.unwrap());
        assert_eq!(
            row_count(&db, "class_course").await,
            0,
            "the attachment rows must go with the course"
        );
        assert_eq!(
            one_i64(&db, "SELECT COALESCE(class_course_count, 0) FROM class_group".to_string()).await,
            0,
            "…and each class must get its count back"
        );
        // Which is the whole point: the class is deletable again.
        assert!(delete(&db, class).await.unwrap());
    }
}
