use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    EXAM_QUESTION_TABLE, MAX_CHOICE_TEXT_LEN, MAX_QUESTION_CHOICES, MAX_QUESTION_TEXT_LEN,
    MIN_QUESTION_CHOICES,
};
use crate::database::Database;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam::ExamId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_question_kind, validate_question_points, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamQuestionId(RecordId);

impl ExamQuestionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`: the
    /// id *is* the question's presentation order ([`ExamQuestion::list_for_exam`]
    /// sorts `id ASC`), and a random low half scrambles a burst of saves that
    /// lands inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(EXAM_QUESTION_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EXAM_QUESTION_TABLE, key))
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
pub struct QuestionText(String);

impl QuestionText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("text", value, MAX_QUESTION_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated question kind: `choice` (pick one option, auto-scorable) or
/// `text` (free text, judged by the grader).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct QuestionKind(String);

impl QuestionKind {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_question_kind(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated question weight in the auto-score, held to
/// `[MIN_QUESTION_POINTS, MAX_QUESTION_POINTS]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct QuestionPoints(i64);

impl QuestionPoints {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_question_points(value)?;
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

/// One option's text: non-blank, at most `MAX_CHOICE_TEXT_LEN` characters.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChoiceText(String);

impl ChoiceText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("choice", value, MAX_CHOICE_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An option's stable identity — a server-minted ULID, never reused, never
/// taken from client input (it becomes part of an image row's record id and of
/// a URL path, so a client-shaped value would be an injection surface).
///
/// This id is the whole point of the choice remodel. `correct`, the per-option
/// pictures, and a student's `selected` all name an option *by id*, so
/// reordering the list or deleting an option moves none of them. They used to
/// be parallel arrays keyed by position, where any edit to the list silently
/// re-pointed the others.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChoiceId(String);

impl ChoiceId {
    /// Plain `Ulid::new()` deliberately: options are ordered by their position
    /// in the stored `Vec`, never by id, so nothing here reads the id as a
    /// clock — it only has to be unique.
    fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One option of a choice question: its identity plus its text.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Choice {
    id: ChoiceId,
    text: ChoiceText,
}

impl Choice {
    pub fn get_id(&self) -> &ChoiceId {
        &self.id
    }

    pub fn get_text(&self) -> &ChoiceText {
        &self.text
    }
}

/// One option as a client submits it. `id` is the option's *key within this
/// payload* — see [`QuestionSpec::try_new`] for how it is resolved.
#[derive(Debug, Clone)]
pub struct ChoiceInput {
    pub id: Option<String>,
    pub text: String,
}

/// The kind-dependent fields of a question, validated as a unit — they only
/// make sense together. `try_new` is the sole constructor, so a `QuestionSpec`
/// in hand always satisfies:
///
/// - `text` → no `choices`, no `correct` (the grader judges the answer),
/// - `choice` → 2–10 `choices` plus a `correct` naming one of them.
#[derive(Debug, Clone)]
pub struct QuestionSpec {
    kind: QuestionKind,
    choices: Option<Vec<Choice>>,
    correct: Option<ChoiceId>,
}

impl QuestionSpec {
    /// Validate the kind-dependent fields as a unit and resolve each submitted
    /// option to a stable [`ChoiceId`].
    ///
    /// **The identity rule, one sentence:** every submitted choice has a key —
    /// its `id` if one was sent — and a key that matches one of `existing` *is*
    /// that stored option (same id, so its picture survives); any other key
    /// names a brand-new option, whose submitted string is used only to resolve
    /// `correct` inside this payload and is never stored.
    ///
    /// `existing` is the question's stored choices (`&[]` on create, so every
    /// option there is new and every id is minted here). `correct` names one of
    /// the submitted keys.
    ///
    /// Why an unrecognised id mints instead of failing: `correct` has to be
    /// settable on create, where nothing is stored yet, so a client must be
    /// able to label an option the server has never seen. The failure mode is
    /// benign and visible — a stale id degrades to "this is a new option", which
    /// loses that option's picture but can never steal another option's, since
    /// it matches no stored id.
    pub fn try_new(
        kind: QuestionKind,
        choices: Option<Vec<ChoiceInput>>,
        correct: Option<String>,
        existing: &[Choice],
    ) -> Result<Self, ValidationError> {
        let invalid = |field, reason| ValidationError::Invalid { field, reason };
        let (choices, correct) = match (kind.as_str(), choices) {
            ("text", Some(_)) => {
                return Err(invalid("choices", "only choice questions take choices"));
            }
            ("text", None) => {
                if correct.is_some() {
                    return Err(invalid(
                        "correct",
                        "only choice questions take a correct choice",
                    ));
                }
                (None, None)
            }
            (_, None) => return Err(invalid("choices", "required for a choice question")),
            (_, Some(inputs)) => {
                if !(MIN_QUESTION_CHOICES..=MAX_QUESTION_CHOICES).contains(&inputs.len()) {
                    return Err(invalid("choices", "must list 2 to 10 choices"));
                }
                let mut resolved: Vec<Choice> = Vec::with_capacity(inputs.len());
                // Submitted key -> the id it resolved to, so `correct` can name
                // a fresh option by the label the client gave it.
                let mut keys: Vec<(String, ChoiceId)> = Vec::new();
                for input in inputs {
                    let id = match input.id {
                        Some(key) => {
                            if keys.iter().any(|(seen, _)| *seen == key) {
                                return Err(invalid("choices", "duplicate choice id"));
                            }
                            let id = existing
                                .iter()
                                .find(|choice| choice.id.as_str() == key)
                                .map_or_else(ChoiceId::generate, |choice| choice.id.clone());
                            keys.push((key, id.clone()));
                            id
                        }
                        None => ChoiceId::generate(),
                    };
                    resolved.push(Choice {
                        id,
                        text: ChoiceText::try_new(&input.text)?,
                    });
                }
                let Some(correct) = correct else {
                    return Err(invalid("correct", "required for a choice question"));
                };
                let Some((_, correct)) = keys.into_iter().find(|(key, _)| *key == correct) else {
                    return Err(invalid("correct", "must name one of the choices"));
                };
                (Some(resolved), Some(correct))
            }
        };
        Ok(Self {
            kind,
            choices,
            correct,
        })
    }

    /// The stored kind-dependent fields as the validated bundle, bypassing
    /// [`Self::try_new`] — the fields were written through one, so the
    /// invariants already hold. For merge-on-update and for copying a question
    /// between the exam and the bank.
    pub(crate) fn from_stored(
        kind: QuestionKind,
        choices: Option<Vec<Choice>>,
        correct: Option<ChoiceId>,
    ) -> Self {
        Self {
            kind,
            choices,
            correct,
        }
    }

    pub fn get_kind(&self) -> &QuestionKind {
        &self.kind
    }

    /// The validated fields, consumed — for a writer in another module (the
    /// question bank) that stores the same three columns.
    pub fn into_parts(self) -> (QuestionKind, Option<Vec<Choice>>, Option<ChoiceId>) {
        (self.kind, self.choices, self.correct)
    }
}

/// One question of an exam. Order within the exam is the id's ULID order
/// (creation order); the kind-dependent columns always satisfy the
/// [`QuestionSpec`] invariants because every write goes through one. Every
/// question links to a subject of the exam's course — the handlers verify the
/// subject's course matches before any write.
#[derive(Debug, Clone, SurrealValue)]
pub struct ExamQuestion {
    id: ExamQuestionId,
    exam: ExamId,
    subject: SubjectId,
    text: QuestionText,
    kind: QuestionKind,
    points: QuestionPoints,
    choices: Option<Vec<Choice>>,
    correct: Option<ChoiceId>,
    /// The bank template this question was created *from*, if it was inserted
    /// out of the bank. Written once, at insert.
    #[surreal(default)]
    from_bank: Option<BankQuestionId>,
    /// The bank template most recently created *by saving this question* into
    /// the bank, if any. Written by [`Self::link_banked_as`], repointed by every
    /// repeat save. Never set by an insert-from-bank: the two directions are
    /// separate columns precisely so "came from the bank" can't be misread as
    /// "already saved to the bank".
    #[surreal(default)]
    banked_as: Option<BankQuestionId>,
}

impl ExamQuestion {
    pub fn get_id(&self) -> &ExamQuestionId {
        &self.id
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
    }

    pub fn get_subject(&self) -> &SubjectId {
        &self.subject
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

    pub fn get_choices(&self) -> Option<&[Choice]> {
        self.choices.as_deref()
    }

    pub fn get_correct(&self) -> Option<&ChoiceId> {
        self.correct.as_ref()
    }

    pub fn get_from_bank(&self) -> Option<&BankQuestionId> {
        self.from_bank.as_ref()
    }

    pub fn get_banked_as(&self) -> Option<&BankQuestionId> {
        self.banked_as.as_ref()
    }

    /// The stored kind-dependent fields as the validated bundle (for
    /// merge-on-update, and for copying into the bank).
    pub fn spec(&self) -> QuestionSpec {
        QuestionSpec::from_stored(
            self.kind.clone(),
            self.choices.clone(),
            self.correct.clone(),
        )
    }

    pub async fn create(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        Self::insert(exam, subject, text, points, spec, None, db).await
    }

    /// Like [`Self::create`], but records the bank template this question was
    /// instantiated from (`POST …/questions/from-bank/{bid}`).
    pub async fn create_from_bank(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source: BankQuestionId,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        Self::insert(exam, subject, text, points, spec, Some(source), db).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        from_bank: Option<BankQuestionId>,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        let question = ExamQuestion {
            id: ExamQuestionId::generate(),
            exam: exam.clone(),
            subject,
            text,
            points,
            kind: spec.kind,
            choices: spec.choices,
            correct: spec.correct,
            from_bank,
            // An insert never banks anything: only a to-bank save writes this.
            banked_as: None,
        };
        let created: Option<ExamQuestion> =
            db.create(question.id.record()).content(question).await?;
        created.ok_or_else(|| AppError::Internal("failed to create exam question".into()))
    }

    pub async fn read(
        id: &ExamQuestionId,
        db: &Database,
    ) -> Result<Option<ExamQuestion>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The exam's questions in presentation order (ULID ids sort by creation).
    pub async fn list_for_exam(
        exam: &ExamId,
        db: &Database,
    ) -> Result<Vec<ExamQuestion>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_question WHERE exam = $ex ORDER BY id ASC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamQuestion>>(0)?)
    }

    /// Whether any question anywhere references `subject` — the gate that
    /// blocks deleting a subject still in use.
    pub async fn any_for_subject(subject: &SubjectId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM exam_question WHERE subject = $subject LIMIT 1")
            .bind(("subject", subject.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    pub async fn update(
        mut self,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        self.subject = subject;
        self.text = text;
        self.points = points;
        self.kind = spec.kind;
        self.choices = spec.choices;
        self.correct = spec.correct;
        // whole-row-save-ok: callers hold EXAM_LOCK.write() across the read and this write
        let updated: Option<ExamQuestion> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Point the question's `banked_as` at the bank template it was just saved
    /// into (`POST …/questions/{qid}/to-bank`). The mirror direction of the
    /// `from_bank` [`Self::create_from_bank`] writes, and deliberately a
    /// *different* column: a question inserted from the bank has not been saved
    /// to it, and one field for both would make the client claim it was.
    /// Overwrites any earlier link: repeat saves mint a new template and the
    /// newest one wins.
    ///
    /// Field-scoped write, unlike [`Self::update`]: the caller awaits a bank
    /// insert plus the whole blob-copy loop between reading this row and
    /// linking it, so the row it holds is long stale by now — a whole-row save
    /// would silently revert whatever landed in that window. (The caller takes
    /// `EXAM_LOCK.read()` around this call, which is what keeps
    /// [`Self::update`]'s whole-row save from clobbering the link in the other
    /// direction; the lease guards the ordering, not the staleness.)
    pub async fn link_banked_as(
        self,
        template: BankQuestionId,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        let mut result = db
            .query("UPDATE $id SET banked_as = $bank RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("bank", template.record()))
            .await?
            .check()?;
        result
            .take::<Vec<ExamQuestion>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Delete the question and cascade-remove its answers and image rows, so
    /// neither can point at a missing question. The image *blobs* are the web
    /// layer's to remove — it collects their names before calling this.
    pub async fn delete(self, db: &Database) -> Result<ExamQuestion, AppError> {
        db.query(
            "DELETE exam_answer WHERE question = $q;
             DELETE question_image WHERE question = $q;",
        )
        .bind(("q", self.id.record()))
        .await?
        .check()?;
        let deleted: Option<ExamQuestion> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    /// A question constructed without a database, for pure-function tests.
    #[cfg(test)]
    pub fn test_new(exam: &ExamId, text: &str, points: i64, spec: QuestionSpec) -> ExamQuestion {
        ExamQuestion {
            id: ExamQuestionId::generate(),
            exam: exam.clone(),
            subject: SubjectId::generate(),
            text: QuestionText::try_new(text).unwrap(),
            points: QuestionPoints::try_new(points).unwrap(),
            kind: spec.kind,
            choices: spec.choices,
            correct: spec.correct,
            from_bank: None,
            banked_as: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(value: &str) -> QuestionKind {
        QuestionKind::try_new(value).unwrap()
    }

    /// Two options keyed `a` / `b` — the client-side labels a form sends for
    /// rows the server has never seen.
    fn two_choices() -> Option<Vec<ChoiceInput>> {
        Some(vec![
            ChoiceInput {
                id: Some("a".into()),
                text: "yes".into(),
            },
            ChoiceInput {
                id: Some("b".into()),
                text: "no".into(),
            },
        ])
    }

    fn spec(
        choices: Option<Vec<ChoiceInput>>,
        correct: Option<&str>,
    ) -> Result<QuestionSpec, ValidationError> {
        QuestionSpec::try_new(kind("choice"), choices, correct.map(str::to_string), &[])
    }

    /// A teacher saving several questions back to back gets them back in that
    /// order: `list_for_exam` sorts `id ASC`, so the ids minted inside one
    /// millisecond have to sort in mint order. Revert `generate` to
    /// `Ulid::new()` and this fails — the low 80 bits are redrawn per id, so a
    /// same-tick burst comes out shuffled.
    #[tokio::test]
    async fn ids_sort_in_creation_order() {
        let ids: Vec<String> = (0..500)
            .map(|_| ExamQuestionId::generate().key().to_string())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn text_is_required() {
        assert!(QuestionText::try_new("What is 2 + 2?").is_ok());
        assert!(QuestionText::try_new("").is_err());
        assert!(QuestionText::try_new("   ").is_err());
        assert!(QuestionText::try_new(&"x".repeat(2_001)).is_err());
    }

    #[tokio::test]
    async fn kind_must_be_known() {
        for value in ["choice", "text"] {
            assert_eq!(QuestionKind::try_new(value).unwrap().as_str(), value);
        }
        assert!(QuestionKind::try_new("essay").is_err());
    }

    #[tokio::test]
    async fn points_range_is_enforced() {
        assert_eq!(QuestionPoints::try_new(1).unwrap().as_i64(), 1);
        assert_eq!(QuestionPoints::try_new(100).unwrap().as_i64(), 100);
        assert!(QuestionPoints::try_new(0).is_err());
        assert!(QuestionPoints::try_new(101).is_err());
    }

    #[tokio::test]
    async fn choice_text_is_required() {
        assert!(ChoiceText::try_new("yes").is_ok());
        assert!(ChoiceText::try_new("").is_err());
        assert!(ChoiceText::try_new("   ").is_err());
        assert!(ChoiceText::try_new(&"x".repeat(501)).is_err());
    }

    #[tokio::test]
    async fn spec_invariants_hold() {
        // Text questions carry neither choices nor a correct choice.
        assert!(QuestionSpec::try_new(kind("text"), None, None, &[]).is_ok());
        assert!(QuestionSpec::try_new(kind("text"), two_choices(), None, &[]).is_err());
        assert!(QuestionSpec::try_new(kind("text"), None, Some("a".into()), &[]).is_err());

        // Choice questions need both.
        assert!(spec(two_choices(), Some("a")).is_ok());
        assert!(spec(two_choices(), Some("b")).is_ok());
        assert!(spec(None, None).is_err());
        assert!(spec(None, Some("a")).is_err());
        assert!(spec(two_choices(), None).is_err());
    }

    #[tokio::test]
    async fn spec_correct_must_name_a_submitted_choice() {
        assert!(spec(two_choices(), Some("c")).is_err());
        assert!(spec(two_choices(), Some("")).is_err());
        // An unlabelled option cannot be marked correct — there is nothing to
        // name it by until the server's minted id comes back in the response.
        let unlabelled = Some(vec![
            ChoiceInput {
                id: None,
                text: "yes".into(),
            },
            ChoiceInput {
                id: None,
                text: "no".into(),
            },
        ]);
        assert!(spec(unlabelled, Some("a")).is_err());
    }

    #[tokio::test]
    async fn spec_mints_a_distinct_id_per_choice() {
        let built = spec(two_choices(), Some("b")).unwrap();
        let (_, choices, correct) = built.into_parts();
        let choices = choices.unwrap();
        // Client labels never reach storage.
        assert!(choices.iter().all(|c| c.get_id().as_str().len() == 26));
        assert_ne!(choices[0].get_id(), choices[1].get_id());
        // `correct` resolved to the *second* option's minted id, by label.
        assert_eq!(correct.as_ref(), Some(choices[1].get_id()));
    }

    /// The heart of the remodel: an id that matches a stored option keeps that
    /// option's identity, so reordering and deleting move nothing.
    #[tokio::test]
    async fn existing_ids_are_preserved_across_an_edit() {
        let stored = spec(two_choices(), Some("a"))
            .unwrap()
            .into_parts()
            .1
            .unwrap();
        let (first, second) = (stored[0].get_id().clone(), stored[1].get_id().clone());

        // Resubmit reordered, renaming the first option's text, and marking the
        // option that used to be second as correct.
        let edited = QuestionSpec::try_new(
            kind("choice"),
            Some(vec![
                ChoiceInput {
                    id: Some(second.as_str().into()),
                    text: "no".into(),
                },
                ChoiceInput {
                    id: Some(first.as_str().into()),
                    text: "YES".into(),
                },
            ]),
            Some(second.as_str().into()),
            &stored,
        )
        .unwrap();
        let (_, choices, correct) = edited.into_parts();
        let choices = choices.unwrap();
        assert_eq!(choices[0].get_id(), &second);
        assert_eq!(choices[1].get_id(), &first);
        assert_eq!(choices[1].get_text().as_str(), "YES");
        assert_eq!(correct.as_ref(), Some(&second));
    }

    #[tokio::test]
    async fn spec_rejects_a_duplicate_choice_id() {
        let dupes = Some(vec![
            ChoiceInput {
                id: Some("a".into()),
                text: "yes".into(),
            },
            ChoiceInput {
                id: Some("a".into()),
                text: "no".into(),
            },
        ]);
        assert!(spec(dupes, Some("a")).is_err());
    }

    #[tokio::test]
    async fn spec_choice_count_is_bounded() {
        let n = |count: usize| {
            Some(
                (0..count)
                    .map(|i| ChoiceInput {
                        id: Some(i.to_string()),
                        text: "option".into(),
                    })
                    .collect::<Vec<_>>(),
            )
        };
        assert!(spec(n(1), Some("0")).is_err());
        assert!(spec(n(2), Some("0")).is_ok());
        assert!(spec(n(10), Some("0")).is_ok());
        assert!(spec(n(11), Some("0")).is_err());
    }

    /// Generated coverage of the §2-REVISED identity rules. The example tests
    /// above each pin one hand-picked payload; these generate arbitrary stored
    /// sets and arbitrary submissions over them (matching keys, stale keys,
    /// `null` ids, duplicates, blank and oversized texts) and assert the rules
    /// as one predicate, because every bug this remodel exists to kill lived in
    /// a *relation* between a submitted option and a stored one.
    mod props {
        use super::*;
        use proptest::prelude::*;

        /// A stored question's options, built the only way they can be: through
        /// `try_new` on create, so the ids are real minted ULIDs.
        fn stored_set(n: usize) -> Vec<Choice> {
            let inputs = (0..n)
                .map(|i| ChoiceInput {
                    id: Some(format!("s{i}")),
                    text: format!("stored {i}"),
                })
                .collect();
            QuestionSpec::try_new(kind("choice"), Some(inputs), Some("s0".into()), &[])
                .unwrap()
                .into_parts()
                .1
                .unwrap()
        }

        /// How a submitted option names itself: as a stored option, as some
        /// other string (a client-minted label, a stale id, junk), or not at all.
        #[derive(Debug, Clone)]
        enum Key {
            Stored(usize),
            Other(String),
            Null,
        }

        fn key() -> impl Strategy<Value = Key> {
            prop_oneof![
                4 => (0usize..10).prop_map(Key::Stored),
                3 => prop_oneof![
                    Just(String::new()),
                    Just("new:1".to_string()),
                    "[a-z0-9]{1,5}",
                    Just("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
                ].prop_map(Key::Other),
                1 => Just(Key::Null),
            ]
        }

        fn text() -> impl Strategy<Value = String> {
            prop_oneof![
                6 => "[a-z ]{1,6}",
                1 => Just(String::new()),
                1 => Just("   ".to_string()),
                1 => Just("x".repeat(501)),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(400))]

            #[test]
            fn a_matching_key_is_that_choice_and_nothing_else_can_be(
                stored_n in 2usize..=10,
                submitted in prop::collection::vec((key(), text()), 0..=12),
                correct in prop::option::of(key()),
            ) {
                let stored = stored_set(stored_n);
                let name = |k: &Key| match k {
                    Key::Stored(i) => Some(stored[i % stored_n].id.as_str().to_string()),
                    Key::Other(s) => Some(s.clone()),
                    Key::Null => None,
                };
                let keys: Vec<Option<String>> = submitted.iter().map(|(k, _)| name(k)).collect();
                let texts: Vec<String> = submitted.iter().map(|(_, t)| t.clone()).collect();
                let correct = correct.as_ref().map(|k| name(k).unwrap_or_default());

                let inputs: Vec<ChoiceInput> = keys
                    .iter()
                    .zip(&texts)
                    .map(|(id, text)| ChoiceInput { id: id.clone(), text: text.clone() })
                    .collect();
                let built = QuestionSpec::try_new(
                    kind("choice"),
                    Some(inputs),
                    correct.clone(),
                    &stored,
                );

                // Every way a payload can be rejected, stated independently of
                // the implementation's order of checks.
                let bad_len = !(MIN_QUESTION_CHOICES..=MAX_QUESTION_CHOICES).contains(&submitted.len());
                let mut seen: Vec<&String> = Vec::new();
                let mut dup = false;
                for k in keys.iter().flatten() {
                    if seen.contains(&k) { dup = true; break; }
                    seen.push(k);
                }
                let bad_text = texts.iter().any(|t| t.trim().is_empty() || t.chars().count() > MAX_CHOICE_TEXT_LEN);
                // `correct` must name a *submitted key*; an option sent with
                // `id: null` has no key, so it is unnameable.
                let named = correct.as_ref().is_some_and(|c| keys.iter().flatten().any(|k| k == c));

                if bad_len || dup || bad_text || !named {
                    prop_assert!(built.is_err());
                    return Ok(());
                }

                let (_, choices, resolved) = built.unwrap().into_parts();
                let choices = choices.unwrap();
                prop_assert_eq!(choices.len(), submitted.len());

                let stored_ids: Vec<&str> = stored.iter().map(|c| c.id.as_str()).collect();
                for (i, choice) in choices.iter().enumerate() {
                    // Order and text are passed through untouched.
                    prop_assert_eq!(choice.text.as_str(), texts[i].as_str());
                    match keys[i].as_deref().and_then(|k| stored.iter().find(|c| c.id.as_str() == k)) {
                        // A key matching a stored choice IS that choice: same
                        // id, so its picture stays with it.
                        Some(existing) => prop_assert_eq!(&choice.id, &existing.id),
                        // Any other key (stale, junk, absent) is a NEW option:
                        // freshly minted, so it can never take over a stored
                        // option's identity or its picture.
                        None => {
                            prop_assert_eq!(choice.id.as_str().len(), 26);
                            prop_assert!(!stored_ids.contains(&choice.id.as_str()));
                        }
                    }
                }
                // No id is invented twice.
                let mut out: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
                out.sort_unstable();
                let unique = out.len();
                out.dedup();
                prop_assert_eq!(out.len(), unique);

                // `correct` resolves to the option that carried that key.
                let correct = correct.unwrap();
                let at = keys.iter().position(|k| k.as_ref() == Some(&correct)).unwrap();
                prop_assert_eq!(resolved.as_ref(), Some(&choices[at].id));
            }
        }
    }

    #[tokio::test]
    async fn spec_rejects_blank_or_oversized_choices() {
        let with = |text: String| {
            Some(vec![
                ChoiceInput {
                    id: Some("a".into()),
                    text: "yes".into(),
                },
                ChoiceInput {
                    id: Some("b".into()),
                    text,
                },
            ])
        };
        assert!(spec(with("  ".into()), Some("a")).is_err());
        assert!(spec(with("x".repeat(501)), Some("a")).is_err());
    }
}
