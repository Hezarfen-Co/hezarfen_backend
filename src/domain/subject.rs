use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_SUBJECT_DESCRIPTION_LEN, MAX_SUBJECT_NAME_LEN, SUBJECT_TABLE};
use crate::database::{Database, transaction_with_retry};
use crate::domain::course::CourseId;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubjectId(RecordId);

impl SubjectId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`: the
    /// id *is* the curriculum's order ([`Subject::list_for_course`] sorts
    /// `id ASC`), and a random low half scrambles a burst of saves that lands
    /// inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(SUBJECT_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(SUBJECT_TABLE, key))
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
pub struct SubjectName(String);

impl SubjectName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_SUBJECT_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubjectDescription(String);

impl SubjectDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_SUBJECT_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A subject: one topic of a course's curriculum. Every exam question links to
/// a subject of its exam's course, so results can later be read per topic. The
/// course link is fixed at creation — a subject is course content, and moving
/// it would strand the questions tagged with it.
#[derive(Debug, Clone, SurrealValue)]
pub struct Subject {
    id: SubjectId,
    course: CourseId,
    name: SubjectName,
    description: SubjectDescription,
}

impl Subject {
    pub fn get_id(&self) -> &SubjectId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_name(&self) -> &SubjectName {
        &self.name
    }

    pub fn get_description(&self) -> &SubjectDescription {
        &self.description
    }

    pub async fn create(
        course: &CourseId,
        name: SubjectName,
        description: SubjectDescription,
        db: &Database,
    ) -> Result<Subject, AppError> {
        let subject = Subject {
            id: SubjectId::generate(),
            course: course.clone(),
            name,
            description,
        };
        let created: Option<Subject> = db.create(subject.id.record()).content(subject).await?;
        created.ok_or_else(|| AppError::Internal("failed to create subject".into()))
    }

    pub async fn read(id: &SubjectId, db: &Database) -> Result<Option<Subject>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Several subjects in one query — the bulk half of a list endpoint that
    /// names each row's subject (a read per row would be an N+1).
    pub async fn list_by_ids(ids: &[&SubjectId], db: &Database) -> Result<Vec<Subject>, AppError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
        let mut result = db
            .query("SELECT * FROM subject WHERE id IN $ids")
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Subject>>(0)?)
    }

    /// The course's subjects in curriculum order (ULID ids sort by creation).
    pub async fn list_for_course(
        course: &CourseId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Subject>, i64), AppError> {
        PagedList::new("subject WHERE course = $course", "ORDER BY id ASC")
            .bind("course", course.record())
            .run(limit, offset, db)
            .await
    }

    /// Request-scoped: no lock spans the handler's read and this write, so an
    /// omitted field (`None`) is not written at all. Passing the snapshot's
    /// value back instead would revert a concurrent edit of that field —
    /// scoping the `SET` alone does not prevent that, the values have to come
    /// from the request. Neither column is nullable, so plain `Option` per
    /// field says everything there is to say.
    pub async fn update(
        self,
        name: Option<SubjectName>,
        description: Option<SubjectDescription>,
        db: &Database,
    ) -> Result<Subject, AppError> {
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("description", description)
            .run::<Subject>(db)
            .await
    }

    /// Delete the subject and clear it off every bank template that carried it
    /// as origin metadata — one transaction, so a template can't be left
    /// pointing at a subject that no longer exists.
    ///
    /// Exam questions and homework are *not* cascaded: their `subject` is a
    /// required field that may not be orphaned, so either one still refuses the
    /// delete with a 409. That refusal is the delete's own `WHERE`, read off the
    /// two reference counters this row carries
    /// ([`crate::constant::SUBJECT_QUESTION_COUNT_FIELD`] and its homework
    /// twin), which is what makes it hold against a question created by a
    /// request racing this one — the cross-table `SELECT … LIMIT 1` it replaces
    /// was a count-then-delete no transaction serializes, pinned by three
    /// process-wide locks that could not cover the round trip between them.
    ///
    /// Which of the two blocked is read off the counters *before* the delete,
    /// purely to pick the message; the decision itself was already made by the
    /// `WHERE`.
    ///
    /// The bank's subject is optional metadata, and blocking on it was a dead
    /// end — only the template's owner may re-tag it, so a manager could never
    /// clear their own 409, and a private template raising it leaked its
    /// existence. Its cascade runs *after* the conditional delete, so a refused
    /// delete leaves every template's subject where it was.
    pub async fn delete(self, db: &Database) -> Result<Subject, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 LET $held = (SELECT exam_question_count AS q, homework_count AS h FROM $sub);
                 LET $before = (DELETE $sub
                     WHERE (exam_question_count ?? 0) = 0 AND (homework_count ?? 0) = 0
                     RETURN BEFORE);
                 IF array::len($before) = 0 {
                     THROW IF array::len($held) = 0 { 'subject_missing' }
                         ELSE IF ($held[0].q ?? 0) > 0 { 'subject_questions' }
                         ELSE { 'subject_homework' }
                 };
                 UPDATE bank_question SET subject = NONE WHERE subject = $sub;
                 RETURN $before;
                 COMMIT TRANSACTION;",
            &[("sub".into(), self.id.record().into_value())],
            &["subject_missing", "subject_questions", "subject_homework"],
        )
        .await?;
        // An aborted transaction errors every slot; only the THROW's own slot
        // names the marker (the [`crate::domain::appointment_slot`] treatment),
        // and a lost round is re-sent rather than reported.
        let thrown = |marker: &str| {
            errors
                .values()
                .any(|error| error.to_string().contains(marker))
        };
        if thrown("subject_questions") {
            return Err(AppError::Conflict(
                "exam questions still reference this subject — re-tag or delete them first",
            ));
        }
        if thrown("subject_homework") {
            return Err(AppError::Conflict(
                "homework still references this subject — re-tag or delete it first",
            ));
        }
        if thrown("subject_missing") {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Read through the trailing `RETURN`, not a counted slot — see
        // [`crate::domain::exam::Exam::delete`].
        let slot = result.num_statements().saturating_sub(2);
        let deleted: Option<Subject> = result.take::<Vec<Subject>>(slot)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A teacher entering a curriculum gets it back in the order they typed it:
    /// `list_for_course` sorts `id ASC`, so the ids minted inside one
    /// millisecond have to sort in mint order. Revert `generate` to
    /// `Ulid::new()` and this fails — the low 80 bits are redrawn per id, so a
    /// same-tick burst comes out shuffled.
    #[tokio::test]
    async fn ids_sort_in_creation_order() {
        let ids: Vec<String> = (0..500)
            .map(|_| SubjectId::generate().key().to_string())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn name_is_required() {
        assert!(SubjectName::try_new("Limits").is_ok());
        assert!(SubjectName::try_new("").is_err());
        assert!(SubjectName::try_new("   ").is_err());
        assert!(SubjectName::try_new(&"x".repeat(201)).is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(SubjectDescription::try_new("").is_ok());
        assert!(SubjectDescription::try_new(&"x".repeat(2_001)).is_err());
    }

    /// GUARD, not a retry measurement — read the last paragraph before
    /// trusting this test with the retry. See
    /// [`crate::domain::course::Course::delete`]'s race test for why the rate
    /// is counted rather than asserted per round, and why this needs the real
    /// server and a multi-threaded runtime.
    ///
    /// The racer is [`ExamQuestion::create`], which claims the subject's
    /// question reference *before* it writes the row — a conditional write on
    /// the same record the delete's `WHERE` reads. `Err(Conflict)` (still
    /// referenced), `Err(NotFound)` and the claim's `Validation` miss are all
    /// correct answers; only `AppError::Db` is the defect. What the two stored
    /// counters below assert is that the sweep genuinely straddled the site:
    /// some rounds the claims won, some rounds the delete did.
    ///
    /// What it does *not* prove is the retry. The 500 window here is the delete
    /// passing its `WHERE` and then committing while a claim is in flight, and
    /// this cascade is three statements long — measured at 0 conflicts in 100
    /// raced rounds, and the whole test stays green with
    /// [`crate::database::transaction_with_retry`]'s loop cut to a single
    /// attempt. Widening the sweep, staggering the burst and doubling it to 12
    /// racers all failed to open the window (they only move which side wins).
    /// So this is a status-code-and-cascade guard: a raced delete answers 409 or
    /// 404 and never 500, and a landed question survives it. The retry itself is
    /// measured on [`crate::domain::course::Course::delete`], whose cascade is
    /// long enough to lose a round (1-2 of 20, red under the same mutation).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_a_question_never_answers_500() {
        use crate::domain::exam::ExamId;
        use crate::domain::exam_question::{
            ExamQuestion, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
        };
        let (db, _serialized) = crate::database::init_test_server("subject_delete_race").await;
        let (mut delete_500, mut question_500) = (0, 0);
        let (mut landed, mut wiped) = (0, 0);
        let (mut last_delete, mut last_question) = (String::new(), String::new());
        for round in 0..20 {
            let course = CourseId::generate();
            let exam = ExamId::generate();
            let subject = Subject::create(
                &course,
                SubjectName::try_new("Limits").unwrap(),
                SubjectDescription::try_new("").unwrap(),
                &db,
            )
            .await
            .unwrap();

            let separated = round % 4 == 0;
            let drop_it = {
                let (subject, db) = (subject.clone(), db.clone());
                // One round in four holds the racers back by a clear 2ms so the
                // delete wins outright: the sub-millisecond sweep alone leaves
                // them ahead of it nearly every round (measured 20 to 0), and
                // both counters below have to see a side. The other three keep
                // the sub-ms beat, which is the only spacing that overlaps at
                // all — a whole millisecond either way separates them.
                let beat = if separated {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_micros(round * 53 % 300)
                };
                tokio::spawn(async move {
                    tokio::time::sleep(beat).await;
                    subject.delete(&db).await
                })
            };
            let asks: Vec<_> = (0..6)
                .map(|_| {
                    let (id, db, exam) = (subject.get_id().clone(), db.clone(), exam.clone());
                    let head_start = if separated {
                        std::time::Duration::from_millis(2)
                    } else {
                        std::time::Duration::from_micros(round * 37 % 300)
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(head_start).await;
                        ExamQuestion::create(
                            &exam,
                            id,
                            QuestionText::try_new("why").unwrap(),
                            QuestionPoints::try_new(1).unwrap(),
                            QuestionSpec::try_new(
                                QuestionKind::try_new("text").unwrap(),
                                None,
                                None,
                                &[],
                            )
                            .unwrap(),
                            &db,
                        )
                        .await
                    })
                })
                .collect();
            let drop_it = drop_it.await.unwrap();
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
                last_delete = format!("{drop_it:?}");
            }
            for ask in asks {
                let ask = ask.await.unwrap();
                if matches!(ask, Err(AppError::Db(_))) {
                    question_500 += 1;
                    last_question = format!("{ask:?}");
                }
            }
            // Stored state, and both sides are needed: a question landing means
            // the claim beat the guard, the subject being gone means the delete
            // did. Seeing only one is a run that never swept across the window.
            if Subject::read(subject.get_id(), &db)
                .await
                .unwrap()
                .is_none()
            {
                wiped += 1;
            }
            if !ExamQuestion::list_for_exam(&exam, None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
            {
                landed += 1;
            }
        }
        eprintln!(
            "Subject::delete raced: {delete_500}/20 delete 500s, {question_500} question 500s, \
             {landed} rounds with a question landed / {wiped} wiped"
        );
        assert!(
            landed > 0 && wiped > 0,
            "the sweep never crossed the window ({landed} landed / {wiped} wiped)"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must be refused, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            question_500, 0,
            "a raced question create must retry, not 500: {question_500}/20 rounds, last {last_question}"
        );
    }
}
