//! A class (şube): a named set of students the school manages as one, so a
//! course attach enrolls the whole set at once. The membership and the
//! attachments live in their own tables (`class_member`, `class_course`) and are
//! counted on this row — a class may only be deleted at zero on both, the same
//! stored guard shape courses and terms use.
//!
//! A class links a term exactly like a course does, and claims a reference on it
//! before the link is written. The reference is its *own* column
//! ([`TERM_CLASS_COUNT_FIELD`]) rather than the courses' `course_count`, because
//! boot seeds that one from the course rows alone and would wipe a class's share
//! of it on the next migration.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    CLASS_COURSE_COUNT_FIELD, CLASS_GROUP_TABLE, CLASS_MEMBER_COUNT_FIELD, MAX_CLASS_GRADE_LEN,
    MAX_CLASS_NAME_LEN, TERM_CLASS_COUNT_FIELD,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::cap;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::term::{self, TermId};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

/// The `THROW` marker the delete guard aborts with — a class that still holds
/// students or courses, or a class row that is no longer there.
const LINKS_MARK: &str = "class_links";

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassGroupId(RecordId);

impl ClassGroupId {
    pub fn generate() -> Self {
        Self(RecordId::new(CLASS_GROUP_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(CLASS_GROUP_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassName(String);

impl ClassName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_CLASS_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The school's own label for the year a class sits in ("9", "10-A",
/// "anaokulu"). Free text on purpose — no school's grade ladder is the next
/// one's — and optional: a club-shaped class has no grade at all.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassGrade(String);

impl ClassGrade {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("grade", value, MAX_CLASS_GRADE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One class. `creator` is who made it; the two refcounts behind the delete
/// guard are database-side columns only, so no whole-row save can clobber one
/// (see [`crate::domain::cap`]).
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassGroup {
    id: ClassGroupId,
    creator: UserId,
    name: ClassName,
    grade: Option<ClassGrade>,
    term: Option<TermId>,
}

impl ClassGroup {
    pub fn get_id(&self) -> &ClassGroupId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_name(&self) -> &ClassName {
        &self.name
    }

    pub fn get_grade(&self) -> Option<&ClassGrade> {
        self.grade.as_ref()
    }

    pub fn get_term(&self) -> Option<&TermId> {
        self.term.as_ref()
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    /// Create the class, claiming a reference on the term it links (if any) in
    /// the *same transaction* as the row, exactly as
    /// [`crate::domain::course::Course::create`] does: the claim is a
    /// conditional write on the term row, so it fails when the term is already
    /// gone, it makes the term undeletable the instant this link exists, and no
    /// crash can leave either half without the other.
    pub async fn create(
        creator: &UserId,
        name: ClassName,
        grade: Option<ClassGrade>,
        term: Option<TermId>,
        db: &Database,
    ) -> Result<ClassGroup, AppError> {
        let class = ClassGroup {
            id: ClassGroupId::generate(),
            creator: creator.clone(),
            name,
            grade,
            term,
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

    pub async fn read(id: &ClassGroupId, db: &Database) -> Result<Option<ClassGroup>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every class, newest first.
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassGroup>, i64), AppError> {
        PagedList::new("class_group", "ORDER BY id DESC")
            .run(limit, offset, db)
            .await
    }

    /// Write only the fields the PATCH carried — `None` means the request
    /// omitted it, so the column is left alone rather than re-stated from the
    /// snapshot this struct was read into. `grade` and `term` are nullable, so
    /// they take the outer/inner `Option<Option<_>>`: `None` = omitted (keep),
    /// `Some(None)` = clear.
    ///
    /// A term move claims the new term and releases the old one inside the very
    /// transaction that moves the link, exactly as in
    /// [`crate::domain::course::Course::update`]: both counters and the link
    /// commit together, so no crash can strand a count on a term nothing links.
    pub async fn update(
        self,
        name: Option<ClassName>,
        grade: Option<Option<ClassGrade>>,
        term: Option<Option<TermId>>,
        db: &Database,
    ) -> Result<ClassGroup, AppError> {
        let (claim, release) = term::ref_move(self.term.as_ref(), &term);
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("grade", grade)
            .set("term", term.map(|term| term.map(|term| term.record())))
            .refcount(TERM_CLASS_COUNT_FIELD, claim, release, term::gone_error())
            .run::<ClassGroup>(db)
            .await
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
    pub async fn delete(self, db: &Database) -> Result<bool, AppError> {
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
            &[("class".into(), self.id.record().into_value())],
            &[LINKS_MARK],
        )
        .await?;
        if errors
            .values()
            .any(|error| error.to_string().contains(LINKS_MARK))
        {
            // Still linked or already gone: the guard cannot tell those apart,
            // and only the refusal path pays for the extra read that can.
            return match Self::read(&self.id, db).await? {
                Some(_) => Ok(false),
                None => Err(AppError::NotFound),
            };
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::term::{Term, TermName};

    async fn class_on(term: Option<TermId>, db: &Database) -> ClassGroup {
        ClassGroup::create(
            &UserId::from_key("manager"),
            ClassName::try_new("9-A").unwrap(),
            None,
            term,
            db,
        )
        .await
        .unwrap()
    }

    async fn a_term(db: &Database) -> Term {
        let at = crate::domain::timestamp::Timestamp::from_millis;
        Term::create(TermName::try_new("2026").unwrap(), at(100), at(200), db)
            .await
            .unwrap()
    }

    /// The stored counter, re-read — never off a return value, which the
    /// in-memory engine forges wins on (see [`crate::domain::cap`]).
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

    #[test]
    fn name_is_required_and_grade_is_optional() {
        assert!(ClassName::try_new("9-A").is_ok());
        assert!(ClassName::try_new("").is_err());
        assert!(ClassName::try_new("   ").is_err());
        assert!(ClassName::try_new(&"x".repeat(MAX_CLASS_NAME_LEN + 1)).is_err());
        assert!(ClassGrade::try_new("").is_ok());
        assert!(ClassGrade::try_new("anaokulu").is_ok());
        assert!(ClassGrade::try_new(&"x".repeat(MAX_CLASS_GRADE_LEN + 1)).is_err());
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
            !term.clone().delete(&db).await.unwrap(),
            "two linked classes must refuse the delete"
        );

        patched.update(None, None, Some(None), &db).await.unwrap();
        assert!(
            !term.clone().delete(&db).await.unwrap(),
            "one link is still one link"
        );

        assert!(linked.delete(&db).await.unwrap());
        assert!(
            term.clone().delete(&db).await.unwrap(),
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
                !class.clone().delete(&db).await.unwrap(),
                "{field} above zero must refuse the delete"
            );
            assert!(
                ClassGroup::read(class.get_id(), &db)
                    .await
                    .unwrap()
                    .is_some(),
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
            assert!(class.clone().delete(&db).await.unwrap());
            assert_eq!(
                stored_count("SELECT VALUE class_count ?? 0 FROM term", &db).await,
                0
            );
            let again = class.delete(&db).await;
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
        assert!(term.delete(&db).await.unwrap());

        let error = ClassGroup::create(
            &UserId::from_key("manager"),
            ClassName::try_new("9-A").unwrap(),
            None,
            Some(id),
            &db,
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
    /// [`crate::domain::course::Course`]'s move tests: a term move carries both
    /// counters with the link, and a move to a term that is gone rolls the
    /// release back with the abort (the transaction releases before it claims,
    /// so the old count would be 0 if the abort did not undo it).
    #[tokio::test]
    async fn a_class_term_move_moves_both_counts_or_neither() {
        let db = crate::database::init_mem().await.unwrap();
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let from = a_term(&db).await;
        let to = Term::create(TermName::try_new("2027").unwrap(), at(100), at(200), &db)
            .await
            .unwrap();
        let dead = Term::create(TermName::try_new("2028").unwrap(), at(100), at(200), &db)
            .await
            .unwrap();
        let dead_id = dead.get_id().clone();
        assert!(dead.delete(&db).await.unwrap());
        let class = class_on(Some(from.get_id().clone()), &db).await;

        let error = class
            .clone()
            .update(
                Some(ClassName::try_new("9-B").unwrap()),
                None,
                Some(Some(dead_id)),
                &db,
            )
            .await
            .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
        let stored = ClassGroup::read(class.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(from.get_id()), "the link stays put");
        assert_eq!(stored.get_name().as_str(), "9-A", "…and so does the row");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release must roll back with the abort"
        );

        let moved = class
            .update(None, None, Some(Some(to.get_id().clone())), &db)
            .await
            .unwrap();
        assert_eq!(moved.get_term(), Some(to.get_id()));
        assert_eq!(count_on(from.get_id(), &db).await, 0, "the old term is free");
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
    }

    /// [`crate::domain::course::Course::delete`]'s class sweep: deleting a course
    /// takes its `class_course` attachments with it and gives each class its
    /// count back, or the classes would be undeletable forever over rows that
    /// point at nothing.
    #[tokio::test]
    async fn deleting_a_course_sweeps_its_class_attachments() {
        use crate::domain::course::{Course, CourseDescription, CourseKind, CourseTitle};

        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let course = Course::create(
            &manager,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            None,
            &db,
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

        assert!(course.delete(&db).await.unwrap());
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
        assert!(class.delete(&db).await.unwrap());
    }
}
