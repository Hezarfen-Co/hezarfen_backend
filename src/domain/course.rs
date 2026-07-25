use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_COURSE_DESCRIPTION_LEN, MAX_COURSE_TITLE_LEN};
use crate::database::{COURSE_TABLE, Database};
use crate::domain::field_update::FieldUpdate;
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_course_kind, validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseId(RecordId);

impl CourseId {
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_TABLE, key))
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
pub struct CourseTitle(String);

impl CourseTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_COURSE_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseDescription(String);

impl CourseDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_COURSE_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated course kind: `course` (a regular class — ders), `study` (a
/// supervised study session — etüt), or `club` (a student club — kulüp).
/// Purely a label; all kinds behave identically.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseKind(String);

impl CourseKind {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_course_kind(value)?;
        Ok(Self(value.to_string()))
    }

    /// The classic kind — what every course is unless said otherwise.
    pub fn course() -> Self {
        Self("course".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A course: the unit exams and enrollments hang off. Marks are computed per
/// course, each exam weighted by its kind's settings weight. May belong to an
/// academic term. Comes in three behaviorally identical kinds: `course`,
/// `study` (etüt), and `club` (kulüp). An optional `capacity` caps the roster
/// at enroll time (`None` = unlimited); rows written before the field existed
/// decode as uncapped.
///
/// `creator` owns the course for good — only they (or a manager+) may delete
/// it. `teachers` are the staff a manager assigned to run it: full management
/// rights inside the course, no power to delete it or change the assignment
/// list. Rows written before the field existed decode with nobody assigned.
#[derive(Debug, Clone, SurrealValue)]
pub struct Course {
    id: CourseId,
    creator: UserId,
    #[surreal(default)]
    teachers: Vec<UserId>,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    term: Option<TermId>,
    capacity: Option<i64>,
}

impl Course {
    pub fn get_id(&self) -> &CourseId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &CourseTitle {
        &self.title
    }

    pub fn get_description(&self) -> &CourseDescription {
        &self.description
    }

    pub fn get_kind(&self) -> &CourseKind {
        &self.kind
    }

    pub fn get_term(&self) -> Option<&TermId> {
        self.term.as_ref()
    }

    /// The seat cap enforced at enroll time; `None` = unlimited.
    pub fn get_capacity(&self) -> Option<i64> {
        self.capacity
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    /// The staff assigned to run this course, in assignment order.
    pub fn get_teachers(&self) -> &[UserId] {
        &self.teachers
    }

    /// Whether `user` was assigned to teach this course. Says nothing about
    /// the creator — they own it whether or not they also appear here.
    pub fn is_assigned(&self, user: &UserId) -> bool {
        self.teachers.contains(user)
    }

    pub async fn create(
        creator: &UserId,
        title: CourseTitle,
        description: CourseDescription,
        kind: CourseKind,
        term: Option<TermId>,
        capacity: Option<i64>,
        db: &Database,
    ) -> Result<Course, AppError> {
        let course = Course {
            id: CourseId::generate(),
            creator: creator.clone(),
            teachers: Vec::new(),
            title,
            description,
            kind,
            term,
            capacity,
        };
        let created: Option<Course> = db.create(course.id.record()).content(course).await?;
        created.ok_or_else(|| AppError::Internal("failed to create course".into()))
    }

    pub async fn read(id: &CourseId, db: &Database) -> Result<Option<Course>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query("SELECT * FROM course ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// The courses `user` is enrolled in — the spine of `/courses/me` and the
    /// marks report.
    pub async fn list_enrolled(user: &UserId, db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM course
                 WHERE id IN (SELECT VALUE course FROM enrollment WHERE user = $usr)
                 ORDER BY id DESC",
            )
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// The courses `user` runs — the ones they created plus the ones a manager
    /// assigned them to. A teacher's slice of the catalog.
    pub async fn list_for_teacher(user: &UserId, db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query("SELECT * FROM course WHERE creator = $usr OR $usr IN teachers ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// Load every course behind `ids` (one query) — the join half of the
    /// attendance report's per-course blocks.
    pub async fn list_by_ids(ids: &[CourseId], db: &Database) -> Result<Vec<Course>, AppError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = ids.iter().map(CourseId::record).collect();
        let mut result = db
            .query("SELECT * FROM course WHERE id IN $ids")
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// Request-scoped: `teachers` is written by [`Self::assign_teacher`],
    /// [`Self::unassign_teacher`] and the demotion sweep, and nothing guards
    /// the course row across the handler's read and this write (its `TERM_LOCK`
    /// window guards the *term* it links, and the assign path takes no lock at
    /// all) — so a field the request omitted (`None`) is not written at all.
    /// Sending the snapshot's value back instead would revert a concurrent
    /// edit of that field; scoping the `SET` alone does not stop that, the
    /// values have to come from the request. `term` and `capacity` are
    /// nullable, so they take the outer/inner `Option<Option<_>>`: `None` =
    /// omitted (keep), `Some(None)` = clear.
    pub async fn update(
        self,
        title: Option<CourseTitle>,
        description: Option<CourseDescription>,
        kind: Option<CourseKind>,
        term: Option<Option<TermId>>,
        capacity: Option<Option<i64>>,
        db: &Database,
    ) -> Result<Course, AppError> {
        FieldUpdate::new(self.id.record())
            .set("title", title)
            .set("description", description)
            .set("kind", kind)
            .set("term", term.map(|term| term.map(|term| term.record())))
            .set("capacity", capacity)
            .run::<Course>(db)
            .await
    }

    /// Assign `teacher` to run this course, or return the course untouched if
    /// they already run it — assignment is idempotent, like enrollment.
    /// Field-scoped, and the new list is folded server-side out of the *stored*
    /// one: a course PATCH awaits a term lookup between its read and its write,
    /// so a whole-row save from either side would revert the other. The
    /// `array::distinct` keeps the assignment idempotent even when two requests
    /// name the same teacher at once (the early return only sees a stale row).
    pub async fn assign_teacher(
        self,
        teacher: &UserId,
        db: &Database,
    ) -> Result<Course, AppError> {
        if self.is_assigned(teacher) {
            return Ok(self);
        }
        let mut result = db
            .query(
                "UPDATE $id SET teachers = array::distinct(array::append(teachers, $usr))
                 RETURN AFTER",
            )
            .bind(("id", self.id.record()))
            .bind(("usr", teacher.record()))
            .await?
            .check()?;
        result.take::<Vec<Course>>(0)?.into_iter().next().ok_or(AppError::NotFound)
    }

    /// Drop `teacher` from this course. `None` when they weren't assigned, so
    /// the web layer can answer 404 instead of pretending it removed someone.
    pub async fn unassign_teacher(
        self,
        teacher: &UserId,
        db: &Database,
    ) -> Result<Option<Course>, AppError> {
        if !self.is_assigned(teacher) {
            return Ok(None);
        }
        // Same field-scoped story as [`Self::assign_teacher`]; `-=` drops the
        // one link off the stored list without touching the course's own text.
        let mut result = db
            .query("UPDATE $id SET teachers -= $usr RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("usr", teacher.record()))
            .await?
            .check()?;
        Ok(Some(
            result.take::<Vec<Course>>(0)?.into_iter().next().ok_or(AppError::NotFound)?,
        ))
    }

    /// Strip `user` from every course they were assigned to — the sweep for a
    /// user demoted below `teacher`, who may no longer run anything.
    pub async fn unassign_everywhere(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query("UPDATE course SET teachers -= $usr WHERE $usr IN teachers")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Delete the course and cascade-remove everything inside it: results,
    /// attempts, answers, and question/answer images of its exams, its
    /// homework with their submissions, submission files, and grades, its
    /// enrollments, its sessions with their roll call, its subjects, and the
    /// exams themselves. The children go in one transaction so a crash can't
    /// leave an exam pointing at a deleted course. The image and
    /// homework-file *blobs* are the web layer's to remove — it collects
    /// their names before calling this.
    pub async fn delete(self, db: &Database) -> Result<Course, AppError> {
        db.query(
            "BEGIN TRANSACTION;
             DELETE exam_result WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_attempt WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_answer WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE answer_image WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE question_image WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_question WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE homework_file WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course));
             DELETE homework_submission WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course);
             DELETE homework_result WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course);
             DELETE session_attendance WHERE course = $course;
             DELETE course_session WHERE course = $course;
             DELETE enrollment WHERE course = $course;
             DELETE subject WHERE course = $course;
             DELETE homework WHERE course = $course;
             DELETE exam WHERE course = $course;
             COMMIT TRANSACTION;",
        )
        .bind(("course", self.id.record()))
        .await?
        .check()?;
        let deleted: Option<Course> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(CourseTitle::try_new("algebra").is_ok());
        assert!(CourseTitle::try_new("").is_err());
        assert!(CourseTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(CourseDescription::try_new("").is_ok());
    }

    #[tokio::test]
    async fn kind_is_course_study_or_club() {
        assert!(CourseKind::try_new("course").is_ok());
        assert!(CourseKind::try_new("study").is_ok());
        assert!(CourseKind::try_new("club").is_ok());
        assert!(CourseKind::try_new("etut").is_err());
        assert_eq!(CourseKind::course().as_str(), "course");
    }

    /// The database strips `NONE`-valued optional columns, and every course
    /// row written before the capacity field existed has no `capacity` key at
    /// all — both must decode as an uncapped course.
    #[tokio::test]
    async fn course_decodes_without_capacity_key() {
        use surrealdb::types::Value;

        let course = Course {
            id: CourseId::generate(),
            creator: UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T"),
            teachers: Vec::new(),
            title: CourseTitle::try_new("chess").unwrap(),
            description: CourseDescription::try_new("").unwrap(),
            kind: CourseKind::try_new("club").unwrap(),
            term: None,
            capacity: Some(12),
        };
        let Value::Object(mut object) = course.into_value() else {
            panic!("course must encode as an object");
        };
        object.remove("capacity");
        let decoded = Course::from_value(Value::Object(object)).unwrap();
        assert_eq!(decoded.get_capacity(), None);
    }

    /// Every course row written before teacher assignment existed has no
    /// `teachers` key. The boot backfill fills them in, but a row read before
    /// that lands must still decode — as a course nobody was assigned to,
    /// never as a decode error that 500s the catalog.
    #[tokio::test]
    async fn course_decodes_without_teachers_key() {
        use surrealdb::types::Value;

        let assigned = UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8U");
        let course = Course {
            id: CourseId::generate(),
            creator: UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T"),
            teachers: vec![assigned.clone()],
            title: CourseTitle::try_new("chess").unwrap(),
            description: CourseDescription::try_new("").unwrap(),
            kind: CourseKind::try_new("club").unwrap(),
            term: None,
            capacity: None,
        };
        assert!(course.is_assigned(&assigned));

        let Value::Object(mut object) = course.into_value() else {
            panic!("course must encode as an object");
        };
        object.remove("teachers");
        let decoded = Course::from_value(Value::Object(object)).unwrap();
        assert!(decoded.get_teachers().is_empty());
        assert!(!decoded.is_assigned(&assigned));
    }
}
