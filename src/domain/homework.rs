//! A homework assignment: a teacher hands it out per course, optionally
//! narrowed to a subset of the enrolled students, and every homework is tagged
//! with one of its course's subjects. Students submit against it
//! ([`crate::domain::homework_submission`]) and a teacher grades a status plus
//! an optional mark ([`crate::domain::homework_result`]).
//!
//! `course`, `created_by`, and `created_at` are fixed at creation (the schema
//! marks them `READONLY`): moving a homework between courses would strand the
//! submissions and grades of students not in the target course. `subject` is
//! deliberately *not* readonly — it is re-taggable through PATCH, validated
//! same-course by the web layer, exactly like an exam question's subject.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    HOMEWORK_TABLE, MAX_HOMEWORK_DESCRIPTION_LEN, MAX_HOMEWORK_TITLE_LEN,
    SUBJECT_HOMEWORK_COUNT_FIELD,
};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::course::CourseId;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

/// The answer a link to a subject that is not there gets, on create and on
/// re-tag alike — both claims are conditional writes on the subject row, so a
/// subject a delete already removed matches nothing and the caller says exactly
/// what the web layer's pre-flight lookup would have.
fn subject_gone() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "subject_id",
        reason: "subject does not exist",
    })
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkId(RecordId);

impl HomeworkId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// homework lists `id DESC` (newest first, [`Homework::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(HOMEWORK_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(HOMEWORK_TABLE, key))
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
pub struct HomeworkTitle(String);

impl HomeworkTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_HOMEWORK_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkDescription(String);

impl HomeworkDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_HOMEWORK_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A homework assignment. `assigned` is the optional student subset: `None`
/// (the column absent) and an empty list both mean "the whole course" — see
/// [`Homework::student_sees`]. Because whole-course homework carries no roster,
/// a student who enrolls later is covered automatically; a subset is a fixed
/// snapshot of the students named at assign (or last PATCH) time.
#[derive(Debug, Clone, SurrealValue)]
pub struct Homework {
    id: HomeworkId,
    course: CourseId,
    subject: SubjectId,
    title: HomeworkTitle,
    description: Option<HomeworkDescription>,
    due_at: Timestamp,
    assigned: Option<Vec<UserId>>,
    created_by: UserId,
    created_at: Timestamp,
}

impl Homework {
    pub fn get_id(&self) -> &HomeworkId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_subject(&self) -> &SubjectId {
        &self.subject
    }

    pub fn get_title(&self) -> &HomeworkTitle {
        &self.title
    }

    pub fn get_description(&self) -> Option<&HomeworkDescription> {
        self.description.as_ref()
    }

    pub fn get_due_at(&self) -> Timestamp {
        self.due_at
    }

    /// The assigned student subset, or `None` for a whole-course homework.
    pub fn get_assigned(&self) -> Option<&[UserId]> {
        self.assigned.as_deref()
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Whether `user` is in this homework's audience. A whole-course homework
    /// (`assigned` absent or empty) is visible to every enrolled student; a
    /// subset homework only to the students it names. Callers pair this with an
    /// enrollment check — being named is visibility, not enrollment.
    pub fn student_sees(&self, user: &UserId) -> bool {
        match &self.assigned {
            None => true,
            Some(assigned) => assigned.is_empty() || assigned.contains(user),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the sibling entities' create(field, field, ..) shape"
    )]
    pub async fn create(
        course: &CourseId,
        subject: &SubjectId,
        title: HomeworkTitle,
        description: Option<HomeworkDescription>,
        due_at: Timestamp,
        assigned: Option<Vec<UserId>>,
        created_by: &UserId,
        db: &Database,
    ) -> Result<Homework, AppError> {
        // The subject's reference is taken in the very transaction that writes
        // the row — the exam question's twin
        // ([`crate::domain::exam_question::ExamQuestion`]): the subject delete
        // is refused while this counter is non-zero, so the create and the
        // delete contend on the subject record rather than on a cross-table
        // count neither of them sees the other move, and a crash can no longer
        // strand a claim that would make the subject undeletable forever. A
        // refused claim means the subject is already gone, which is the 400 the
        // web layer's pre-flight check answers with.
        let counted = subject.record();
        let homework = Homework {
            id: HomeworkId::generate(),
            course: course.clone(),
            subject: subject.clone(),
            title,
            description,
            due_at,
            assigned,
            created_by: created_by.clone(),
            created_at: Timestamp::now(),
        };
        let id = homework.id.record();
        match cap::claim_and_create(
            &counted,
            SUBJECT_HOMEWORK_COUNT_FIELD,
            cap::UNLIMITED,
            &id,
            &homework,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(created),
            cap::Claimed::Full => Err(subject_gone()),
            cap::Claimed::Duplicate => Err(AppError::Internal("failed to create homework".into())),
        }
    }

    pub async fn read(id: &HomeworkId, db: &Database) -> Result<Option<Homework>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The course's homework, newest first (ULID ids sort by creation). The web
    /// layer retains only the rows a given student `student_sees`.
    pub async fn list_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework WHERE course = $course ORDER BY id DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Every homework in the system, newest first — the manager+ view of the
    /// cross-course "my homework" list.
    pub async fn list_all(db: &Database) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Every homework of every course in `courses`, newest first (one query) —
    /// the cross-course list over a caller's visible courses. The web layer
    /// still trims each course's rows to what the caller may see (a student to
    /// the ones they `student_sees`).
    pub async fn list_for_courses(
        courses: &[CourseId],
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        if courses.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = courses.iter().map(CourseId::record).collect();
        let mut result = db
            .query("SELECT * FROM homework WHERE course IN $courses ORDER BY id DESC")
            .bind(("courses", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// The homework of `course` that `user` is meant to see — whole-course ones
    /// plus any subset that names them — newest first. Backs a student's (or an
    /// observer's) per-course homework report; mirrors [`Homework::student_sees`]
    /// in SurQL so the filter runs in the database.
    pub async fn list_for_user_in_course(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM homework
                 WHERE course = $course
                   AND (assigned = NONE OR assigned = [] OR $usr IN assigned)
                 ORDER BY id DESC",
            )
            .bind(("course", course.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Re-tag, re-title, re-describe, re-schedule, or re-scope the homework.
    /// Request-scoped: every parameter is `Option`, `None` meaning the PATCH
    /// did not carry that field, so it is not written at all. Handing the
    /// snapshot's value back instead would revert a concurrent edit of that
    /// field — scoping the `SET` alone does not prevent that, the *values* must
    /// come from the request. `course`, `created_by`, and `created_at` are
    /// `READONLY` and never appear in the write.
    ///
    /// `description` and `assigned` are nullable columns, so they take a
    /// *double* option: outer `None` = absent (keep), `Some(None)` = write
    /// `NONE` (clear the description / widen back to the whole course). The web
    /// layer has already re-checked a new `due_at` against now, a new `subject`
    /// against the course, and refused a narrowing that would orphan work.
    pub async fn update(
        self,
        subject: Option<SubjectId>,
        title: Option<HomeworkTitle>,
        description: Option<Option<HomeworkDescription>>,
        due_at: Option<Timestamp>,
        assigned: Option<Option<Vec<UserId>>>,
        db: &Database,
    ) -> Result<Homework, AppError> {
        let assigned = assigned
            .map(|subset| subset.map(|users| users.iter().map(UserId::record).collect::<Vec<_>>()));
        // A re-tag moves a reference: the new subject's claim and the old one's
        // release ride the same transaction as the link write, so no crash can
        // leave a count without its link (the subject would be undeletable
        // forever) or a link without its count. `subject` is required, so the
        // snapshot's value is always the CAS expectation — two PATCHes moving
        // the same homework off the same subject would otherwise both claim
        // their target, and the loser is refused with a 409 instead. The
        // `.refcount` call is unconditional: the CAS is armed by the request
        // *carrying* `subject_id`, not by a counter moving, so a PATCH that
        // re-states the tag its snapshot showed — shifting no counter at all —
        // is still refused when a rival moved the tag in between. A PATCH that
        // carried no `subject_id` arms nothing and writes what it always did.
        let (claim, release) = subject
            .as_ref()
            .filter(|next| **next != self.subject)
            .map(|next| (next.record(), self.subject.record()))
            .unzip();
        FieldUpdate::new(self.id.record())
            .set("subject", subject.map(|subject| subject.record()))
            .set("title", title)
            .set("description", description)
            .set("due_at", due_at)
            .set("assigned", assigned)
            .refcount(
                SUBJECT_HOMEWORK_COUNT_FIELD,
                "subject",
                Some(self.subject.record()),
                claim,
                release,
                subject_gone(),
            )
            .run::<Homework>(db)
            .await
    }

    /// Delete the homework and cascade its submissions, their files, and its
    /// results — one transaction, so a crash can't orphan a submission under a
    /// vanished homework. The submission file *blobs* are the web layer's to
    /// unlink: it collects their names via
    /// [`crate::domain::homework_file::HomeworkFile::file_keys_for_homework`]
    /// before calling this, and removes them after the rows are gone.
    pub async fn delete(self, db: &Database) -> Result<Homework, AppError> {
        let mut result = db
            .query(
                format!(
                    "BEGIN TRANSACTION;
                     DELETE homework_file WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework = $hw);
                     DELETE homework_submission WHERE homework = $hw;
                     DELETE homework_result WHERE homework = $hw;
                     LET $gone = (DELETE $hw RETURN BEFORE);
                     FOR $sub IN ($gone.subject ?? []) {{
                         UPDATE $sub SET {SUBJECT_HOMEWORK_COUNT_FIELD} =
                             math::max([({SUBJECT_HOMEWORK_COUNT_FIELD} ?? 0) - 1, 0])
                     }};
                     RETURN $gone;
                     COMMIT TRANSACTION;"
                ),
            )
            .bind(("hw", self.id.record()))
            .await?
            .check()?;
        // The subject's reference is given back in this same transaction, off
        // what the delete actually removed. Read through the trailing `RETURN`,
        // not a hand-counted slot — see [`crate::domain::exam::Exam::delete`].
        let slot = result.num_statements().saturating_sub(2);
        let deleted: Option<Homework> = result.take::<Vec<Homework>>(slot)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(HomeworkTitle::try_new("Read chapter 3").is_ok());
        assert!(HomeworkTitle::try_new("").is_err());
        assert!(HomeworkTitle::try_new("   ").is_err());
        assert!(HomeworkTitle::try_new(&"x".repeat(201)).is_err());
    }

    #[tokio::test]
    async fn description_is_optional_but_bounded() {
        assert!(HomeworkDescription::try_new("").is_ok());
        assert!(HomeworkDescription::try_new(&"x".repeat(2_001)).is_err());
    }

    #[tokio::test]
    async fn student_sees_covers_whole_course_and_named_subsets() {
        let a = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let b = UserId::from_key("01TESTUSERBBBBBBBBBBBBBBBB");
        let with = |assigned| Homework {
            id: HomeworkId::generate(),
            course: CourseId::from_key("01TESTCOURSEAAAAAAAAAAAAAA"),
            subject: SubjectId::from_key("01TESTSUBJECTAAAAAAAAAAAAA"),
            title: HomeworkTitle::try_new("hw").unwrap(),
            description: None,
            due_at: Timestamp::from_millis(1),
            assigned,
            created_by: a.clone(),
            created_at: Timestamp::from_millis(1),
        };
        // Whole course: absent or empty list means everyone sees it.
        assert!(with(None).student_sees(&a));
        assert!(with(Some(vec![])).student_sees(&b));
        // Subset: only the named students.
        assert!(with(Some(vec![a.clone()])).student_sees(&a));
        assert!(!with(Some(vec![a.clone()])).student_sees(&b));
    }

    use crate::domain::subject::{Subject, SubjectDescription, SubjectName};

    async fn a_subject(name: &str, db: &Database) -> Subject {
        Subject::create(
            &crate::domain::course::a_test_course(db).await,
            SubjectName::try_new(name).unwrap(),
            SubjectDescription::try_new("").unwrap(),
            db,
        )
        .await
        .unwrap()
    }

    async fn homework_on(subject: &SubjectId, db: &Database) -> Homework {
        Homework::create(
            &CourseId::from_key("course"),
            subject,
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
            db,
        )
        .await
        .unwrap()
    }

    /// The stored `homework_count` on one subject, absent counting as zero.
    async fn count_on(subject: &SubjectId, db: &Database) -> i64 {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({SUBJECT_HOMEWORK_COUNT_FIELD} ?? 0) FROM $sub"
            ))
            .bind(("sub", subject.record()))
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

    /// The invariant on the create path: the claim and the row it accounts for
    /// commit together or not at all. A refused create leaves *neither* — no
    /// homework row, and no count stranded on a subject (the subject's delete
    /// guard reads that count, so a stray one makes it undeletable forever).
    #[tokio::test]
    async fn a_refused_create_writes_neither_row_nor_count() {
        let db = crate::database::init_mem().await.unwrap();
        let subject = a_subject("algebra", &db).await;
        let id = subject.get_id().clone();
        subject.delete(&db).await.unwrap();

        let error = Homework::create(
            &CourseId::from_key("course"),
            &id,
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
            &db,
        )
        .await
        .expect_err("a subject that is gone must not be taggable");
        assert!(error.to_string().contains("subject does not exist"));
        assert_eq!(
            rows("SELECT VALUE id FROM homework", &db).await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM subject", &db).await,
            0,
            "…and least of all a count on a subject it just brought back"
        );
    }

    /// The invariant on the PATCH path: a re-tag carries the new subject's
    /// claim and the old one's release with the link itself.
    #[tokio::test]
    async fn a_subject_move_moves_the_count() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        assert_eq!(count_on(from.get_id(), &db).await, 1);

        let moved = homework
            .update(Some(to.get_id().clone()), None, None, None, None, &db)
            .await
            .unwrap();
        assert_eq!(moved.get_subject(), to.get_id());
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the old subject is free"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
        assert!(from.delete(&db).await.is_ok(), "no reference left");
        assert!(
            to.delete(&db).await.is_err(),
            "the reference moved here refuses the delete"
        );
    }

    /// The claim throws inside the same transaction as the link write, so a
    /// move onto a subject that is gone rolls the release back with it: the row
    /// keeps its tag and both counters read as if nothing ran.
    #[tokio::test]
    async fn a_move_to_a_dead_subject_leaves_everything_untouched() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let dead = a_subject("geometry", &db).await;
        let gone = dead.get_id().clone();
        dead.delete(&db).await.unwrap();
        let homework = homework_on(from.get_id(), &db).await;

        let error = homework
            .clone()
            .update(Some(gone.clone()), None, None, None, None, &db)
            .await
            .expect_err("a subject that is gone must not be taggable");
        assert!(error.to_string().contains("subject does not exist"));
        let stored = Homework::read(homework.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_subject(), from.get_id(), "the tag never moved");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release rolled back with the claim"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM subject", &db).await,
            1,
            "the dead subject was not brought back by a count"
        );
        assert_eq!(count_on(&gone, &db).await, 0);
    }

    /// The double-claim guard. Both movers compute their claim and release from
    /// the row as *they* read it, so two PATCHes re-tagging the same homework
    /// off the same subject both release it and both claim their target — two
    /// counts for one link, and the loser's target is undeletable forever. The
    /// second call here runs on the struct read before the first one landed,
    /// which is that race with the interleaving pinned: it must be refused
    /// outright, and the counts must read as if it never ran.
    #[tokio::test]
    async fn a_stale_mover_is_refused_and_claims_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let other = a_subject("calculus", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        let stale = homework.clone();
        homework
            .update(Some(to.get_id().clone()), None, None, None, None, &db)
            .await
            .unwrap();

        let error = stale
            .clone()
            .update(Some(other.get_id().clone()), None, None, None, None, &db)
            .await
            .expect_err("a mover that read a tag it no longer holds must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a 404 or a 500: {error:?}"
        );
        let stored = Homework::read(stale.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_subject(), to.get_id(), "the winner's tag");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "released once, not twice"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once");
        assert_eq!(count_on(other.get_id(), &db).await, 0, "never claimed");
    }

    /// The same race with the counters taken out of it. A PATCH that re-states
    /// the subject its snapshot showed shifts *nothing* — no claim and no
    /// release — so if the guard were armed off the counter move it would not be
    /// armed here at all, and the write would land: the winner's tag silently
    /// dragged back, its claim stranded on a subject nothing points at
    /// (undeletable forever) and the reverted-to subject tagged at a count of
    /// zero (deletable while tagged). The guard is armed by the request
    /// *carrying* the column instead, which is why this is refused.
    #[tokio::test]
    async fn a_stale_re_stater_is_refused_and_reverts_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        let stale = homework.clone();
        homework
            .update(Some(to.get_id().clone()), None, None, None, None, &db)
            .await
            .unwrap();

        let error = stale
            .clone()
            .update(Some(from.get_id().clone()), None, None, None, None, &db)
            .await
            .expect_err("re-stating a tag someone else moved must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a silent 200: {error:?}"
        );
        let stored = Homework::read(stale.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_subject(), to.get_id(), "the winner's tag");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the reverted-to subject must not end up tagged at zero"
        );
        assert_eq!(
            count_on(to.get_id(), &db).await,
            1,
            "…nor the winner's subject counted with nothing pointing at it"
        );

        // A *genuine* no-op re-state — nobody moved underneath it — still lands,
        // and still moves no counter: the CAS passes trivially.
        let fresh = Homework::read(stale.get_id(), &db).await.unwrap().unwrap();
        let same = fresh
            .update(Some(to.get_id().clone()), None, None, None, None, &db)
            .await
            .expect("re-stating the tag actually held is not a race");
        assert_eq!(same.get_subject(), to.get_id());
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "still no counter move"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once, still");
    }
}
