//! A reusable question template in the school-wide question bank. Unlike an
//! [`crate::domain::exam_question::ExamQuestion`], a bank row is fully detached
//! from any exam: no exam FK, so no attempt ever freezes it, and its images
//! live in their own [`crate::domain::bank_question_image`] slot table. Teachers
//! save templates here and later *copy* them into an exam — the copy is a fresh
//! `ExamQuestion` with its own id, images, and answers; the two never share a
//! row. The kind-dependent columns satisfy the [`QuestionSpec`] invariants
//! because every write goes through one.
//!
//! Pure types only: the queries live in [`crate::db::bank_question`], the
//! PATCH re-derive in [`crate::service::bank_question`].

use sqlx::types::Json;

use crate::constant::{BANK_VISIBILITY_PRIVATE, BANK_VISIBILITY_SCHOOL};
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    Choice, ChoiceId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// Who may see a template. `private` — the owner (and admins) alone; `school` —
/// every teacher.
///
/// **The default is `private`, deliberately.** A template carries `correct`, the
/// answer key, and its images: publishing one is an explicit act, never a side
/// effect of saving a question to the bank. A new row defaults to `private`
/// (the column is `TEXT NOT NULL DEFAULT 'private'`), so the bank can't
/// retroactively broadcast anyone's answer keys.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct BankVisibility(String);

impl Default for BankVisibility {
    fn default() -> Self {
        Self(BANK_VISIBILITY_PRIVATE.to_string())
    }
}

impl BankVisibility {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        if value != BANK_VISIBILITY_PRIVATE && value != BANK_VISIBILITY_SCHOOL {
            return Err(ValidationError::Invalid {
                field: "visibility",
                reason: "must be private or school",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the template is published to the whole school.
    pub fn is_school(&self) -> bool {
        self.0 == BANK_VISIBILITY_SCHOOL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct BankQuestionId(uuid::Uuid);

impl BankQuestionId {
    /// A write-ordered id. `list` sorts `id DESC` to mean "newest first", and
    /// a plain random UUID is only millisecond-accurate — templates saved
    /// inside one tick (a to-bank burst, a test loop) would come back
    /// shuffled, so the id comes from [`crate::domain::monotonic_id`] instead.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row — a malformed path param stays a 404, exactly
    /// like a well-formed one that names nothing.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// One template in the shared question bank. `owner` is the teacher who saved
/// it (the only one who may edit or delete it, admins aside); `subject` is
/// origin metadata only — the same-course rule is checked against the caller's
/// subject when a template is copied into an exam, not here.
///
/// `subject` is optional because it is *only* metadata: deleting a subject
/// nulls it school-wide (see [`crate::db::subject::delete`])
/// rather than being blocked by the bank. Blocking would have been a dead end
/// — the bank is owner-or-admin editable, so a manager could not resolve their
/// own 409 — and an existence oracle, since another teacher's *private*
/// template would have raised it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BankQuestion {
    pub(crate) id: BankQuestionId,
    pub(crate) owner: UserId,
    /// The origin subject, or `None` once that subject was deleted.
    pub(crate) subject: Option<SubjectId>,
    pub(crate) text: QuestionText,
    pub(crate) kind: QuestionKind,
    pub(crate) points: QuestionPoints,
    pub(crate) choices: Option<Json<Vec<Choice>>>,
    pub(crate) correct: Option<ChoiceId>,
    /// The exam question this template was saved from, if any.
    pub(crate) source_exam: Option<ExamId>,
    /// Who may read it; defaults to `private` at the column.
    pub(crate) visibility: BankVisibility,
    pub(crate) created_at: Timestamp,
}

impl BankQuestion {
    pub fn get_id(&self) -> &BankQuestionId {
        &self.id
    }

    pub fn get_owner(&self) -> &UserId {
        &self.owner
    }

    pub fn get_subject(&self) -> Option<&SubjectId> {
        self.subject.as_ref()
    }

    pub fn get_text(&self) -> &QuestionText {
        &self.text
    }

    pub fn get_kind(&self) -> &QuestionKind {
        &self.kind
    }

    pub fn get_points(&self) -> QuestionPoints {
        self.points
    }

    pub fn get_correct(&self) -> Option<&ChoiceId> {
        self.correct.as_ref()
    }

    pub fn get_source_exam(&self) -> Option<&ExamId> {
        self.source_exam.as_ref()
    }

    pub fn get_visibility(&self) -> &BankVisibility {
        &self.visibility
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// The stored kind-dependent fields as the validated bundle (for
    /// merge-on-update, and for copying into an exam). Bypasses `try_new`: the
    /// fields were written through a `QuestionSpec`, so the invariants already
    /// hold — and a round-trip through `try_new` would mint *new* choice ids,
    /// detaching the copy's `correct` and option pictures from its choices.
    pub fn spec(&self) -> QuestionSpec {
        QuestionSpec::from_stored(
            self.kind.clone(),
            self.choices.as_ref().map(|json| json.0.clone()),
            self.correct.clone(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        id: BankQuestionId,
        owner: UserId,
        subject: Option<SubjectId>,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_exam: Option<ExamId>,
        visibility: BankVisibility,
        created_at: Timestamp,
    ) -> BankQuestion {
        let (kind, choices, correct) = spec.into_parts();
        BankQuestion {
            id,
            owner,
            subject,
            text,
            kind,
            points,
            choices: choices.map(Json),
            correct,
            source_exam,
            visibility,
            created_at,
        }
    }
}
