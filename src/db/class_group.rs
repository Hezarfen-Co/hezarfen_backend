//! The `class_group` table: the row, its term reference claimed through
//! [`crate::db::cap`] in the same transaction as the link, and the 0/0 delete
//! guard. The archived-term guard and the route-facing wrappers live in
//! [`crate::service::class_group`].

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{CLASS_COURSE_COUNT_FIELD, CLASS_MEMBER_COUNT_FIELD, TERM_CLASS_COUNT_FIELD};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId, ClassName};
use crate::domain::term::{self, TermId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// The `THROW` marker the delete guard aborts with — a class that still holds
/// students or courses, or a class row that is no longer there.
const LINKS_MARK: &str = "class_links";

/// Create the class, claiming a reference on the term it links (if any) in
/// the *same transaction* as the row, exactly as
/// [`crate::db::course::create`] does: the claim is a
/// conditional write on the term row, so it fails when the term is already
/// gone, it makes the term undeletable the instant this link exists, and no
/// crash can leave either half without the other.
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: ClassName,
    grade: Option<ClassGrade>,
    term: Option<TermId>,
    teacher: Option<UserId>,
) -> Result<ClassGroup, AppError> {
    let class = ClassGroup {
        id: ClassGroupId::generate(),
        creator: creator.clone(),
        name,
        grade,
        term,
        teacher,
    };
    let id = class.id.record();
    let Some(term) = class.term.clone() else {
        let created: Option<ClassGroup> = db.create(id).content(class).await?;
        return created.ok_or_else(|| AppError::Internal("failed to create class".into()));
    };
    match cap::claim_and_create(
        &term.record(),
        TERM_CLASS_COUNT_FIELD,
        cap::UNLIMITED,
        &id,
        &class,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        // Uncapped, so "full" can only mean the conditional write matched no
        // term row at all — the claim doubles as the existence check.
        cap::Claimed::Full => Err(term::gone_error()),
        // Unreachable: the id is a ULID this call just generated.
        cap::Claimed::Duplicate => Err(AppError::Internal("failed to create class".into())),
    }
}

pub async fn read(db: &Database, id: &ClassGroupId) -> Result<Option<ClassGroup>, AppError> {
    Ok(db.select(id.record()).await?)
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
        None => PagedList::new("class_group", "ORDER BY id DESC"),
        // A gradeless class stores no `grade` key at all (SurrealDB drops a
        // key valued NONE), and an absent field reads back as NONE — so the
        // one comparison covers both spellings.
        Some(None) => PagedList::new("class_group WHERE grade IS NONE", "ORDER BY id DESC"),
        Some(Some(grade)) => PagedList::new("class_group WHERE grade = $grade", "ORDER BY id DESC")
            .bind("grade", grade.as_str().to_string()),
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
    let expected = class.term.as_ref().map(TermId::record);
    FieldUpdate::new(class.id.record())
        .set("name", name)
        .set("grade", grade)
        .set("term", term.map(|term| term.map(|term| term.record())))
        // Not refcounted: a homeroom assignment is a label, so it rides the
        // plain `set` path and never arms the term CAS.
        .set(
            "teacher",
            teacher.map(|teacher| teacher.map(|teacher| teacher.record())),
        )
        .refcount(
            TERM_CLASS_COUNT_FIELD,
            "term",
            expected,
            claim,
            release,
            term::gone_error(),
        )
        .run::<ClassGroup>(db)
        .await
}

/// The classes `ids` names, in no particular order — the join behind
/// "which class section is this student in", where the ids come from
/// `class_member` rows already paged. Ids that name no row are simply
/// absent.
pub async fn list_by_ids(db: &Database, ids: &[ClassGroupId]) -> Result<Vec<ClassGroup>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let records: Vec<RecordId> = ids.iter().map(ClassGroupId::record).collect();
    let mut result = db
        .query("SELECT * FROM class_group WHERE id IN $ids")
        .bind(("ids", records))
        .await?
        .check()?;
    Ok(result.take::<Vec<ClassGroup>>(0)?)
}

/// Every class section at one grade label, in no particular order — what a
/// grade blueprint pumps. Unpaged on purpose: the caller is reconciling all
/// of them, and a page would silently stock only the first window.
pub async fn list_for_grade(
    db: &Database,
    grade: &ClassGrade,
) -> Result<Vec<ClassGroup>, AppError> {
    let mut result = db
        .query("SELECT * FROM class_group WHERE grade = $grade")
        .bind(("grade", grade.as_str().to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ClassGroup>>(0)?)
}

/// Strip `user` from every class they were the homeroom teacher of — the
/// sweep for a user demoted below `teacher`, who may no longer hold one.
/// The mirror of [`crate::db::course::unassign_everywhere`];
/// nothing is counted on this column, so there is no reference to give back.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    db.query("UPDATE class_group SET teacher = NONE WHERE teacher = $usr")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(())
}

/// Delete the class and give its term reference back. Nothing cascades: a
/// class that still holds students or courses is refused outright, because
/// dropping it silently would leave the enrollments it pumped behind with
/// nothing left to sweep them.
///
/// `false` = refused, nothing was written. Both counts are read off the
/// class's own row, so the check and the delete are one conditional write on
/// one record — a member or attach racing this either claims first (and the
/// delete is refused) or finds the row gone (and is refused itself).
/// `Err(NotFound)` keeps the answer a concurrent *delete* used to get.
pub async fn delete(db: &Database, class: ClassGroup) -> Result<bool, AppError> {
    // Parenthesized `??` throughout: `n ?? 0 = 0` parses as `n ?? (0 = 0)`,
    // which is truthy for every row and would delete a class still in use.
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $gone = (DELETE $class WHERE ({CLASS_MEMBER_COUNT_FIELD} ?? 0) = 0 \
             AND ({CLASS_COURSE_COUNT_FIELD} ?? 0) = 0 RETURN BEFORE);
         IF array::len($gone) = 0 {{ THROW '{LINKS_MARK}' }};
         FOR $row IN $gone {{
             IF $row.term != NONE {{
                 UPDATE $row.term SET {TERM_CLASS_COUNT_FIELD} = \
                     math::max([({TERM_CLASS_COUNT_FIELD} ?? 0) - 1, 0]);
             }};
         }};
         COMMIT TRANSACTION;"
    );
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the marker, and a lost
    // round is re-sent rather than reported (see [`transaction_with_retry`]).
    let (_, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[("class".into(), class.id.record().into_value())],
        &[LINKS_MARK],
    )
    .await?;
    if errors
        .values()
        .any(|error| error.to_string().contains(LINKS_MARK))
    {
        // Still linked or already gone: the guard cannot tell those apart,
        // and only the refusal path pays for the extra read that can.
        return match read(db, &class.id).await? {
            Some(_) => Ok(false),
            None => Err(AppError::NotFound),
        };
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::term::{Term, TermName};

    async fn class_on(term: Option<TermId>, db: &Database) -> ClassGroup {
        create(
            db,
            &UserId::from_key("manager"),
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
        create(
            db,
            &UserId::from_key("manager"),
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

    /// The stored counter, re-read — never off a return value, which the
    /// in-memory engine forges wins on (see [`crate::db::cap`]).
    async fn stored_count(sql: &str, db: &Database) -> i64 {
        let mut result = db.query(sql).await.unwrap().check().unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .first()
            .copied()
            .unwrap()
    }

    /// The stored `class_count` on one term, absent counting as zero.
    async fn count_on(term: &TermId, db: &Database) -> i64 {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({TERM_CLASS_COUNT_FIELD} ?? 0) FROM $term"
            ))
            .bind(("term", term.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .first()
            .copied()
            .unwrap_or(0)
    }

    /// How many rows `sql` selects ids for.
    async fn rows(sql: &str, db: &Database) -> usize {
        let mut result = db.query(sql).await.unwrap().check().unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    /// The homeroom teacher survives a create, is set and cleared by a PATCH,
    /// and — the part a `.content(self)` save would break — is left alone by a
    /// PATCH of another field. A row written before the column exists carries
    /// no key at all, which must read back as `None`, not fail the decode.
    #[tokio::test]
    async fn the_homeroom_teacher_is_stored_set_cleared_and_left_alone() {
        let db = crate::database::init_mem().await.unwrap();
        let ada = UserId::from_key("ada");
        let boole = UserId::from_key("boole");

        let bare = class_of(None, &db).await;
        assert_eq!(teacher_of(bare.get_id(), &db).await, None);
        let held = class_of(Some(ada.clone()), &db).await;
        assert_eq!(teacher_of(held.get_id(), &db).await, Some(ada.clone()));

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

        let moved = update(&db, renamed, None, None, None, Some(Some(boole.clone())))
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
            Some(Some(UserId::from_key("ada"))),
        )
        .await
        .unwrap();
        assert_eq!(again.get_name().as_str(), "9-B");

        // A row that predates the column: absent key, not NULL.
        db.query("UPDATE $class UNSET teacher")
            .bind(("class", bare.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(teacher_of(bare.get_id(), &db).await, None);
    }

    /// The demotion sweep: a user who drops below `teacher` is cleared from
    /// *every* class they homeroomed, and nobody else's is touched.
    #[tokio::test]
    async fn unassign_everywhere_clears_only_that_users_classes() {
        let db = crate::database::init_mem().await.unwrap();
        let ada = UserId::from_key("ada");
        let boole = UserId::from_key("boole");
        let first = class_of(Some(ada.clone()), &db).await;
        let second = class_of(Some(ada.clone()), &db).await;
        let other = class_of(Some(boole.clone()), &db).await;
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
        let db = crate::database::init_mem().await.unwrap();
        let term = a_term(&db).await;

        let linked = class_on(Some(term.get_id().clone()), &db).await;
        let patched = class_on(Some(term.get_id().clone()), &db).await;
        assert_eq!(
            stored_count("SELECT VALUE class_count ?? 0 FROM term", &db).await,
            2,
            "classes must count on class_count, not course_count"
        );
        assert_eq!(
            stored_count("SELECT VALUE course_count ?? 0 FROM term", &db).await,
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
        for field in [CLASS_MEMBER_COUNT_FIELD, CLASS_COURSE_COUNT_FIELD] {
            let db = crate::database::init_mem().await.unwrap();
            let term = a_term(&db).await;
            let class = class_on(Some(term.get_id().clone()), &db).await;
            db.query(format!("UPDATE $class SET {field} = 1"))
                .bind(("class", class.get_id().record()))
                .await
                .unwrap()
                .check()
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
                stored_count("SELECT VALUE class_count ?? 0 FROM term", &db).await,
                1,
                "…the term reference least of all"
            );

            // Back to zero, and the same class deletes and releases the term.
            db.query(format!("UPDATE $class SET {field} = 0"))
                .bind(("class", class.get_id().record()))
                .await
                .unwrap()
                .check()
                .unwrap();
            assert!(delete(&db, class.clone()).await.unwrap());
            assert_eq!(
                stored_count("SELECT VALUE class_count ?? 0 FROM term", &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let term = a_term(&db).await;
        let id = term.get_id().clone();
        assert!(crate::db::term::delete(&db, term).await.unwrap());

        let error = create(
            &db,
            &UserId::from_key("manager"),
            ClassName::try_new("9-A").unwrap(),
            None,
            Some(id),
            None,
        )
        .await
        .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
        assert_eq!(
            rows("SELECT VALUE id FROM class_group", &db).await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM term", &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let from = a_term(&db).await;
        let to = crate::db::term::create(&db, TermName::try_new("2027").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let dead = crate::db::term::create(&db, TermName::try_new("2028").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let dead_id = dead.get_id().clone();
        assert!(crate::db::term::delete(&db, dead).await.unwrap());
        let class = class_on(Some(from.get_id().clone()), &db).await;

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
            Some(Some(to.get_id().clone())),
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
        let db = crate::database::init_mem().await.unwrap();
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let from = a_term(&db).await;
        let to = crate::db::term::create(&db, TermName::try_new("2027").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let other = crate::db::term::create(&db, TermName::try_new("2028").unwrap(), at(100), at(200))
            .await
            .unwrap();
        let class = class_on(Some(from.get_id().clone()), &db).await;
        let stale = class.clone();
        update(
            &db,
            class,
            None,
            None,
            Some(Some(to.get_id().clone())),
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
            Some(Some(other.get_id().clone())),
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
            Some(Some(from.get_id().clone())),
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
            Some(Some(to.get_id().clone())),
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

        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
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
        db.query(format!(
            "CREATE class_course SET class = $class, course = $course, attached_by = $usr;
             UPDATE $class SET {CLASS_COURSE_COUNT_FIELD} = 1;"
        ))
        .bind(("class", class.get_id().record()))
        .bind(("course", course.get_id().record()))
        .bind(("usr", manager.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

        assert!(crate::db::course::delete(&db, course).await.unwrap());
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
            0,
            "the attachment rows must go with the course"
        );
        assert_eq!(
            stored_count(
                &format!("SELECT VALUE {CLASS_COURSE_COUNT_FIELD} ?? 0 FROM class_group"),
                &db
            )
            .await,
            0,
            "…and each class must get its count back"
        );
        // Which is the whole point: the class is deletable again.
        assert!(delete(&db, class).await.unwrap());
    }
}
