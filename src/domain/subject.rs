use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_SUBJECT_DESCRIPTION_LEN, MAX_SUBJECT_NAME_LEN, SUBJECT_TABLE};
use crate::database::Database;
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
        let mut result = db
            .query(
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
            )
            .bind(("sub", self.id.record()))
            .await?;
        // An aborted transaction errors every slot; only the THROW's own slot
        // names the marker (the [`crate::domain::appointment_slot`] treatment).
        let mut errors = result.take_errors();
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
}
