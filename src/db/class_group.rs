//! The `class_group` table: the row, its academic-year reference claimed
//! through the [`crate::db::cap`] shapes in the same statement as the link,
//! and the 0/0 delete guard. The archived-year guard and the route-facing
//! wrappers live in [`crate::service::class_group`].

use crate::constant::{ACADEMIC_YEAR_CLASS_COUNT_FIELD, CLASS_GROUP_TABLE};
use crate::database::{Database, tx_with_retry};
use crate::db::field_update::{FieldUpdate, Refcount};
use crate::db::page::PagedList;
use crate::domain::academic_year::AcademicYearId;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId, ClassName};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The refusal a section write naming a year that is not there gets — the mirror
/// of [`crate::domain::term::gone_error`], one layer up the calendar.
fn year_gone() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "year",
        reason: "academic year does not exist",
    })
}

/// What a PATCH's `year` field owes the refcounts: the year to claim and the
/// one to give back. Covers all three moves — set (none→some), move
/// (some→other) and clear (some→none) — and moves nothing for a PATCH that
/// omitted the field or re-stated the link it already had. The same shape
/// [`crate::domain::term::ref_move`] gives the term link.
fn year_ref_move(
    current: Option<&AcademicYearId>,
    patch: &Option<Option<AcademicYearId>>,
) -> (Option<AcademicYearId>, Option<AcademicYearId>) {
    match patch {
        None => (None, None),
        Some(next) if next.as_ref() == current => (None, None),
        Some(next) => (*next, current.copied()),
    }
}

/// Create the class, claiming a reference on the academic year it links (if
/// any) in the *same statement* as the row, exactly as
/// [`crate::db::course::create`] did for the term: the claim is a conditional
/// write on the year row, so it fails when the year is already gone, it makes
/// the year undeletable the instant this link exists, and — the claim and the
/// insert being one CTE — no crash and no refused claim can leave either half
/// without the other.
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: ClassName,
    grade: Option<ClassGrade>,
    year: Option<AcademicYearId>,
    teacher: Option<UserId>,
) -> Result<ClassGroup, AppError> {
    let id = ClassGroupId::generate();
    match &year {
        Some(year) => {
            // The claim and the insert are one statement: atomic without an
            // explicit transaction, exactly like every other
            // single-statement guard.
            let written = sqlx::query_as!(
                ClassGroup,
                r#"WITH seat AS (
                       UPDATE academic_year SET class_count = class_count + 1
                        WHERE id = $1
                        RETURNING 1)
                   INSERT INTO class_group (id, creator, name, grade, year, teacher)
                   SELECT $2, $3, $4, $5, $1, $6 WHERE EXISTS (SELECT 1 FROM seat)
                   RETURNING id AS "id: ClassGroupId", creator AS "creator: UserId",
                     name AS "name: ClassName", grade AS "grade: ClassGrade",
                     year AS "year: AcademicYearId", teacher AS "teacher: UserId""#,
                year.uuid(),
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
                // write matched no year row at all — the claim doubles as the
                // existence check.
                Ok(None) => Err(year_gone()),
                // The year row is locked by the claim's own UPDATE, so this
                // is unreachable; a defensible answer beats a 500.
                Err(e) if crate::database::foreign_key_violation(&e) => Err(year_gone()),
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
                r#"INSERT INTO class_group (id, creator, name, grade, year, teacher)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   RETURNING id AS "id: ClassGroupId", creator AS "creator: UserId",
                     name AS "name: ClassName", grade AS "grade: ClassGrade",
                     year AS "year: AcademicYearId", teacher AS "teacher: UserId""#,
                id.uuid(),
                creator.uuid(),
                name.as_str(),
                grade.as_ref().map(ClassGrade::as_str),
                year.as_ref().map(AcademicYearId::uuid),
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
                  year AS "year: AcademicYearId", teacher AS "teacher: UserId"
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
/// snapshot this struct was read into. `grade`, `year` and `teacher` are
/// nullable, so they take the outer/inner `Option<Option<_>>`: `None` =
/// omitted (keep), `Some(None)` = clear.
///
/// A year move claims the new year and releases the old one inside the very
/// transaction that moves the link, exactly as in
/// [`crate::db::course::update`] did for the term: both counters and the link
/// commit together, so no crash can strand a count on a year nothing links.
pub async fn update(
    db: &Database,
    class: ClassGroup,
    name: Option<ClassName>,
    grade: Option<Option<ClassGrade>>,
    year: Option<Option<AcademicYearId>>,
    teacher: Option<Option<UserId>>,
) -> Result<ClassGroup, AppError> {
    let (claim, release) = year_ref_move(class.year.as_ref(), &year);
    let expected = class.year.as_ref().map(AcademicYearId::uuid);
    FieldUpdate::new(CLASS_GROUP_TABLE, class.id.uuid())
        .set("name", name.map(|name| name.as_str().to_string()))
        .set(
            "grade",
            grade.map(|grade| grade.map(|grade| grade.as_str().to_string())),
        )
        .set("year", year.map(|year| year.map(|year| year.uuid())))
        // Not refcounted: a homeroom assignment is a label, so it rides the
        // plain `set` path and never arms the year CAS.
        .set(
            "teacher",
            teacher.map(|teacher| teacher.map(|teacher| teacher.uuid())),
        )
        .refcount(Refcount {
            counter_table: "academic_year",
            counter_field: ACADEMIC_YEAR_CLASS_COUNT_FIELD,
            link: "year",
            expected,
            claim: claim.map(|year| year.uuid()),
            release: release.map(|year| year.uuid()),
            refused: year_gone(),
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
                  year AS "year: AcademicYearId", teacher AS "teacher: UserId"
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
                  year AS "year: AcademicYearId", teacher AS "teacher: UserId"
           FROM class_group WHERE grade = $1"#,
        grade.as_str()
    )
    .fetch_all(db)
    .await?;
    Ok(classes)
}

/// Every class of one academic year, newest first — the rollover's input list
/// ([`crate::service::academic_year::rollover`]). Unpaged for the same reason
/// [`list_for_grade`] is: the caller reconciles all of them.
pub async fn list_for_year(
    db: &Database,
    year: &AcademicYearId,
) -> Result<Vec<ClassGroup>, AppError> {
    let classes = sqlx::query_as!(
        ClassGroup,
        r#"SELECT id AS "id: ClassGroupId", creator AS "creator: UserId",
                  name AS "name: ClassName", grade AS "grade: ClassGrade",
                  year AS "year: AcademicYearId", teacher AS "teacher: UserId"
           FROM class_group WHERE year = $1 ORDER BY id"#,
        year.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(classes)
}

/// Every class `user` is the homeroom teacher of, newest first — the same
/// order [`list_all`] gives, since this is a narrowing of that list and not a
/// second one.
///
/// It is the read behind "which sections do I run": the homeroom teacher of a
/// section may act on every instance of it (D10, the third arm of
/// [`crate::service::class_course::ensure_instance_teacher`]) without being a
/// member of it or assigned to the instance — and without a read like this the
/// section would be invisible to them, so their own instances' exams,
/// sessions and homework could not be listed at all.
///
/// Unpaged like [`list_for_grade`] and [`list_for_year`]: the caller resolves
/// a *visible set*, and a page would silently hide the sections past the
/// window. A user who homerooms nothing — including one every sweep has
/// cleared (`unassign_everywhere`) — simply gets an empty list.
pub async fn list_for_teacher(db: &Database, user: &UserId) -> Result<Vec<ClassGroup>, AppError> {
    let classes = sqlx::query_as!(
        ClassGroup,
        r#"SELECT id AS "id: ClassGroupId", creator AS "creator: UserId",
                  name AS "name: ClassName", grade AS "grade: ClassGrade",
                  year AS "year: AcademicYearId", teacher AS "teacher: UserId"
           FROM class_group WHERE teacher = $1 ORDER BY id DESC"#,
        user.uuid()
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

/// Delete the class and give its academic-year reference back. A class that
/// still holds students or courses is refused outright, because dropping it
/// silently would leave the enrollments it pumped behind with nothing left to
/// sweep them. What the guard cannot see is the *history* standing beside the
/// live rows, and that is what this delete sweeps first: every `class_member`
/// stint of the class (a soft-left student holds no seat, so the 0/0 guard
/// passes while their row still stands, and `class_member.class` would refuse
/// the delete as a `23503`), plus the provenance tags naming it from rows that
/// outlive it — a rollover copy's `source_class_group` and a handed-over
/// enrollment's `source`, both hard foreign keys into this table. Dropping a
/// section takes its own history with it; a row that merely *remembers* the section
/// keeps standing, untagged. An event aimed at the class keeps standing with
/// `audience_class` cleared — the course twin's documented outcome
/// (`db::course::delete`): the roster resolves live, so it simply reads empty.
///
/// `false` = refused, nothing was written — not even the sweeps below. The
/// guard locks the class row (`FOR UPDATE`) and reads its own two counters, so
/// a member or an attach racing this either claims first (and the delete is
/// refused, untouched) or finds the row gone (and is refused itself).
/// `Err(NotFound)` keeps the answer a concurrent *delete* used to get — and
/// unlike the old one-statement guard, this one tells the two apart without a
/// second read. The row lock is what makes the sweeps safe to run before the
/// final delete: no counter can move under this transaction, so the verdict
/// this read settled is still the verdict when the children are gone.
pub async fn delete(db: &Database, class: ClassGroup) -> Result<bool, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // The guard and the lock, in one read — the shape
        // [`crate::db::course::delete`] uses. A refused delete must leave the
        // class's history exactly as it found it, so nothing may be swept
        // until this verdict is in.
        let guard = sqlx::query!(
            r#"SELECT class_member_count, class_course_count, year AS "year: uuid::Uuid"
                 FROM class_group WHERE id = $1 FOR UPDATE"#,
            class.id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(guard) = guard else {
            return Err(AppError::NotFound);
        };
        if guard.class_member_count != 0 || guard.class_course_count != 0 {
            return Ok(false);
        }
        // Events aimed at this class keep standing, audience cleared — the
        // hard FK on `event.audience_class` would otherwise refuse the delete
        // outright, and a dangling audience resolves to an empty roster.
        sqlx::query!(
            r#"UPDATE event SET audience_class = NULL WHERE audience_class = $1"#,
            class.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // The roster history goes with the section. The 0/0 guard counts *live*
        // stints, so a class whose last student soft-left passes it with the
        // history row still standing — and `class_member.class` is a hard
        // foreign key, which would answer the delete below as a `23503`.
        sqlx::query!(
            r#"DELETE FROM class_member WHERE class = $1"#,
            class.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // …and the same key from the *other* side: a stint another section holds
        // remembers where it came from (a rollover copy's `source_class_group`)
        // and an enrollment a class wrote remembers which class pumped it
        // (its `source`). Both are hard keys into this row, so both would
        // refuse the delete. The rows outlive the section and stay; only the tag
        // goes.
        sqlx::query!(
            r#"UPDATE class_member SET source_class_group = NULL WHERE source_class_group = $1"#,
            class.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE enrollment SET source = NULL WHERE source = $1"#,
            class.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // Last, and FK-silent: every child row whose key names this class is
        // either taken or untagged by now.
        sqlx::query!(r#"DELETE FROM class_group WHERE id = $1"#, class.id.uuid())
            .execute(&mut *tx)
            .await?;
        if let Some(year) = guard.year {
            sqlx::query!(
                r#"UPDATE academic_year
                      SET class_count = GREATEST(class_count - 1, 0)
                    WHERE id = $1"#,
                year
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

    use crate::domain::academic_year::{AcademicYear, AcademicYearName};

    /// A real `app_user` row: creator, teacher, and attached-by are foreign
    /// keys now, so every fixture participant is a row, not a fabricated id.
    async fn a_named_user(db: &Database, username: &str) -> UserId {
        // The username is unique: a second call for the same name must adopt
        // the existing row, not collide with it.
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at, role) \
             VALUES ($1, $2, 0, 'teacher') ON CONFLICT DO NOTHING",
        )
        .bind(UserId::generate().uuid())
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

    /// A real academic year: the section's year link is a foreign key now, and
    /// the class-count claim it carries is what these tests are about.
    async fn a_year(name: &str, db: &Database) -> AcademicYear {
        let at = crate::domain::timestamp::Timestamp::from_millis;
        crate::db::academic_year::create(
            db,
            &a_named_user(db, "manager").await,
            AcademicYearName::try_new(name).unwrap(),
            at(100),
            at(200),
            Vec::new(),
        )
        .await
        .unwrap()
    }

    async fn class_on(year: Option<AcademicYearId>, db: &Database) -> ClassGroup {
        let manager = a_named_user(db, "manager").await;
        create(
            db,
            &manager,
            ClassName::try_new("9-A").unwrap(),
            None,
            year,
            None,
        )
        .await
        .unwrap()
    }

    /// A class with a homeroom teacher, no year.
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

    /// The stored `class_count` on one academic year, absent counting as zero.
    async fn count_on(year: &AcademicYearId, db: &Database) -> i64 {
        one_i64(
            db,
            format!(
                "SELECT COALESCE(class_count, 0) FROM academic_year WHERE id = '{}'",
                year.uuid()
            ),
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

    /// The bite test for the class half of the academic-year delete guard
    /// (D3): a year is undeletable while a *section* links it, on its own
    /// column, and every way that link can end gives the reference back.
    /// Claiming into `term_count` instead would pass the first assert and
    /// fail the roundtrip through the year's own delete.
    #[tokio::test]
    async fn a_year_is_deletable_only_once_no_class_links_it() {
        let (db, _leases) = crate::database::init_test_db().await;
        let year = a_year("2026-2027", &db).await;

        let linked = class_on(Some(*year.get_id()), &db).await;
        let patched = class_on(Some(*year.get_id()), &db).await;
        assert_eq!(
            one_i64(
                &db,
                "SELECT COALESCE(class_count, 0) FROM academic_year".to_string()
            )
            .await,
            2,
            "classes must count on class_count, not term_count"
        );
        assert_eq!(
            one_i64(
                &db,
                "SELECT COALESCE(term_count, 0) FROM academic_year".to_string()
            )
            .await,
            0,
            "the terms' counter is seeded from term rows and must stay untouched"
        );
        assert!(
            !crate::db::academic_year::delete(&db, year.clone())
                .await
                .unwrap(),
            "two linked classes must refuse the delete"
        );

        update(&db, patched, None, None, Some(None), None)
            .await
            .unwrap();
        assert!(
            !crate::db::academic_year::delete(&db, year.clone())
                .await
                .unwrap(),
            "one link is still one link"
        );

        assert!(delete(&db, linked).await.unwrap());
        assert!(
            crate::db::academic_year::delete(&db, year.clone())
                .await
                .unwrap(),
            "the last link gone, the year may go"
        );
        let again = crate::db::academic_year::delete(&db, year).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second delete is a 404, not a refusal: {again:?}"
        );
    }

    /// The bite test for the class delete guard, both arms: either count above
    /// zero refuses, having written nothing — the year reference least of all,
    /// which a refusal that released it would strand.
    #[tokio::test]
    async fn a_class_with_members_or_courses_refuses_to_delete() {
        for field in ["class_member_count", "class_course_count"] {
            let (db, _leases) = crate::database::init_test_db().await;
            let year = a_year("2026-2027", &db).await;
            let class = class_on(Some(*year.get_id()), &db).await;
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
                count_on(year.get_id(), &db).await,
                1,
                "…the year reference least of all"
            );

            // Back to zero, and the same class deletes and releases the year.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE class_group SET {field} = 0 WHERE id = $1"
            )))
            .bind(class.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
            assert!(delete(&db, class.clone()).await.unwrap());
            assert_eq!(count_on(year.get_id(), &db).await, 0);
            let again = delete(&db, class).await;
            assert!(
                matches!(again, Err(AppError::NotFound)),
                "a second delete is a 404, not a refusal: {again:?}"
            );
        }
    }

    /// The class half of the invariant on the create path: a refused create
    /// leaves neither the row nor a count stranded on a year (which the year's
    /// delete guard reads, so a stray one would make it undeletable forever).
    #[tokio::test]
    async fn a_refused_create_writes_neither_row_nor_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let year = a_year("2026-2027", &db).await;
        let id = *year.get_id();
        assert!(crate::db::academic_year::delete(&db, year).await.unwrap());

        let error = create(
            &db,
            &a_named_user(&db, "manager").await,
            ClassName::try_new("9-A").unwrap(),
            None,
            Some(id),
            None,
        )
        .await
        .expect_err("a year that is gone must not be linkable");
        assert!(error.to_string().contains("academic year does not exist"));
        assert_eq!(
            row_count(&db, "class_group").await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            row_count(&db, "academic_year").await,
            0,
            "…and least of all a count on a year it just brought back"
        );
    }

    /// The class mirror of
    /// [`crate::db::course`]'s move tests: a year move carries the count with
    /// the link, and a move to a year that is gone rolls the release back with
    /// the abort (the transaction releases before it claims, so the old count
    /// would be 0 if the abort did not undo it).
    #[tokio::test]
    async fn a_class_year_move_moves_the_count_or_neither() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_year("2026-2027", &db).await;
        let to = a_year("2027-2028", &db).await;
        let dead = a_year("2028-2029", &db).await;
        let dead_id = *dead.get_id();
        assert!(crate::db::academic_year::delete(&db, dead).await.unwrap());
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
        .expect_err("a year that is gone must not be linkable");
        assert!(error.to_string().contains("academic year does not exist"));
        let stored = read(&db, class.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_year(), Some(from.get_id()), "the link stays put");
        assert_eq!(stored.get_name().as_str(), "9-A", "…and so does the row");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release must roll back with the abort"
        );

        let moved = update(&db, class, None, None, Some(Some(*to.get_id())), None)
            .await
            .unwrap();
        assert_eq!(moved.get_year(), Some(to.get_id()));
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the old year is free"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
    }

    /// The class twin of [`crate::db::course`]'s
    /// `a_stale_mover_is_refused_and_claims_nothing` and
    /// `a_stale_re_stater_is_refused_and_reverts_nothing`, in one: both
    /// PATCHes compute their counter move from the row as *they* read it, so a
    /// second one running on the pre-move struct would claim a second seat for
    /// one link (the mover) or drag the link back and strand the winner's claim
    /// (the re-stater, which shifts no counter at all and so is only ever
    /// stopped by a guard armed off the *carried column*). Both must be refused
    /// with the counts reading as if they never ran.
    #[tokio::test]
    async fn a_stale_class_year_write_is_refused_and_moves_no_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_year("2026-2027", &db).await;
        let to = a_year("2027-2028", &db).await;
        let other = a_year("2028-2029", &db).await;
        let class = class_on(Some(*from.get_id()), &db).await;
        let stale = class.clone();
        update(&db, class, None, None, Some(Some(*to.get_id())), None)
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
        assert_eq!(stored.get_year(), Some(to.get_id()), "the winner's link");
        assert_eq!(count_on(from.get_id(), &db).await, 0, "released once");
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once");
        assert_eq!(count_on(other.get_id(), &db).await, 0, "never claimed");

        // A genuine no-op re-state still lands and still moves nothing.
        let same = update(&db, stored, None, None, Some(Some(*to.get_id())), None)
            .await
            .expect("re-stating the link the row really holds is not a conflict");
        assert_eq!(same.get_year(), Some(to.get_id()));
        assert_eq!(count_on(to.get_id(), &db).await, 1, "still one seat");
    }

    /// The delete guard counts *live* stints, and a soft-left student holds
    /// none — so the class passes it with the history row still standing, and
    /// `class_member.class` (a hard foreign key) is what used to answer this
    /// delete, as a `23503` the route turns into a 500. The roster history
    /// goes with the section it belongs to.
    #[tokio::test]
    async fn a_soft_left_member_does_not_block_the_delete_and_goes_with_it() {
        use crate::db::class_pump::{self, Attached};

        let (db, _leases) = crate::database::init_test_db().await;
        let manager = a_named_user(&db, "manager").await;
        let class = class_of(None, &db).await;
        // The pump's member axis claims the `student` role on the user row,
        // and a stint is a real write, not a row fabricated into place.
        let student = a_named_user(&db, "ada").await;
        sqlx::query("UPDATE app_user SET role = 'student' WHERE id = $1")
            .bind(student.uuid())
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            class_pump::add_member(&db, class.get_id(), &student, &manager)
                .await
                .unwrap(),
            Attached::Made(_)
        ));

        assert_eq!(
            class_pump::leave_member(&db, class.get_id(), &student)
                .await
                .unwrap(),
            1,
            "the one live stint is the one the leave ends"
        );
        // Exactly what the guard is blind to: the counters read zero, the
        // history does not.
        assert_eq!(
            one_i64(
                &db,
                format!(
                    "SELECT class_member_count FROM class_group WHERE id = '{}'",
                    class.get_id().uuid()
                )
            )
            .await,
            0
        );
        assert_eq!(row_count(&db, "class_member").await, 1);

        assert!(
            delete(&db, class.clone()).await.unwrap(),
            "a soft-left member must not block the delete"
        );
        assert_eq!(
            row_count(&db, "class_member").await,
            0,
            "…and the roster history goes with the şube"
        );
        assert!(
            matches!(delete(&db, class).await, Err(AppError::NotFound)),
            "a second delete is still a 404"
        );
    }
}
