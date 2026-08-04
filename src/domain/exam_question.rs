use std::collections::HashSet;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    EXAM_QUESTION_TABLE, MAX_CHOICE_TEXT_LEN, MAX_QUESTION_CHOICES, MAX_QUESTION_TEXT_LEN,
    MIN_QUESTION_CHOICES, SUBJECT_QUESTION_COUNT_FIELD,
};
use crate::database::Database;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::cap;
use crate::domain::exam::ExamId;
use crate::domain::exam_attempt::ExamAttempt;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_question_kind, validate_question_points, validate_required};

/// The `THROW` markers the folded counter moves abort with: the subject the
/// reference was to be claimed on is gone, the question being patched is gone,
/// and the subject this move started from is no longer the one the handler
/// read. File-local like every other marker set (`cap`'s, `subject`'s).
const SUBJECT_MARK: &str = "question_subject_gone";
const ROW_MARK: &str = "question_row_gone";
const STALE_MARK: &str = "question_stale_move";

/// The one answer for a subject that isn't there — a claim missing it and the
/// web layer's pre-flight lookup missing it are the same 400.
fn dead_subject() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "subject_id",
        reason: "subject does not exist",
    })
}

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
        let counted = subject.record();
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
        // The freeze gate and the subject's reference ride in the same
        // transaction as the insert: a question cannot appear under an exam
        // somebody has already started, and the claim that accounts for it
        // cannot outlive a row that never landed. Claiming in its own query
        // (with a release on failure, as this did) leaves a window where a
        // crash strands the count — and the subject delete is conditioned on
        // that count reading zero, so a stranded one makes the subject
        // undeletable forever. A missed claim means the subject is already gone
        // — the same 400 the web layer's pre-flight check answers with.
        //
        // Re-sendable despite the `CREATE`: a lost round aborts having written
        // nothing and `$id` is a ULID minted once per call, so the re-send
        // cannot answer "already exists" (there is no UNIQUE index on
        // `exam_question`) — the one thing the retry cannot survive.
        let id = question.id.record();
        // One counter write in flight at a time, like every other counter write.
        let _guard = cap::counter_lock().await;
        let mut result = ExamAttempt::write_unfrozen_with(
            exam,
            &format!(
                "LET $seat = (UPDATE $subject SET {SUBJECT_QUESTION_COUNT_FIELD} = \
                 ({SUBJECT_QUESTION_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
                 IF array::len($seat) = 0 {{ THROW '{SUBJECT_MARK}' }};
                 CREATE $id CONTENT $question;"
            ),
            vec![
                ("subject".into(), counted.into_value()),
                ("id".into(), id.into_value()),
                ("question".into(), question.into_value()),
            ],
            vec![(SUBJECT_MARK, dead_subject())],
            db,
        )
        .await?;
        // Counted off the statements that actually ran rather than a fixed
        // slot, so folding another gate in above can never mis-read the row.
        let slot = result.num_statements().saturating_sub(2);
        result
            .take::<Vec<ExamQuestion>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to create exam question".into()))
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
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ExamQuestion>, i64), AppError> {
        PagedList::new("exam_question WHERE exam = $ex", "ORDER BY id ASC")
            .bind("ex", exam.record())
            .run(limit, offset, db)
            .await
    }

    /// The ids of `exam`'s questions that share a bank template with a question
    /// under one of `live` — the questions two exams hold identical `correct`
    /// for, because the bank copies the key into every instantiation. Keyed on
    /// the template, never on the exam, so only the overlapping questions are
    /// named. Empty when `live` is empty.
    ///
    /// A question links to a template through *either* column and both must be
    /// read, on both sides of the join: `from_bank` is the template it was
    /// instantiated from, `banked_as` the template minted by saving it into the
    /// bank ([`Self::link_banked_as`]). A question authored by hand in exam A
    /// and then saved to the bank holds only `banked_as`, while its copy in
    /// exam B holds only `from_bank` — matching `from_bank` to `from_bank` saw
    /// neither and leaked A's key while B was live. A single coalesced key per
    /// row is not enough either: a question instantiated from one template and
    /// re-saved as another carries *both*, and only the second one may be the
    /// shared link.
    ///
    /// So: collect every template id reachable from a live exam by either
    /// column, then name any of `exam`'s questions pointing at one by either
    /// column. `$shared` is built from `!= NONE` filters, so it never holds a
    /// `NONE` for an unlinked question's absent column to match against.
    pub async fn list_shared_with(
        exam: &ExamId,
        live: &[ExamId],
        db: &Database,
    ) -> Result<HashSet<String>, AppError> {
        if live.is_empty() {
            return Ok(HashSet::new());
        }
        let mut result = db
            .query(
                "LET $shared = array::union(
                   (SELECT VALUE from_bank FROM exam_question
                    WHERE exam IN $live AND from_bank != NONE),
                   (SELECT VALUE banked_as FROM exam_question
                    WHERE exam IN $live AND banked_as != NONE));
                 SELECT VALUE id FROM exam_question
                 WHERE exam = $ex AND (from_bank IN $shared OR banked_as IN $shared)",
            )
            .bind(("ex", exam.record()))
            .bind((
                "live",
                live.iter().map(ExamId::record).collect::<Vec<RecordId>>(),
            ))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<ExamQuestionId>>(1)?
            .iter()
            .map(|id| id.key().to_string())
            .collect())
    }

    /// Write the editable fields, refused outright once the exam has an
    /// attempt — the freeze gate is part of this transaction, not a check the
    /// caller made a moment earlier under a lock.
    ///
    /// Field-scoped, no longer a whole-row save. The row also carries
    /// `from_bank`/`banked_as`, which [`Self::link_banked_as`] writes from a
    /// *different* request: re-stating this snapshot's copy of them would
    /// revert a to-bank save that landed in between. That is exactly what the
    /// caller's `EXAM_LOCK.write()` used to order (inside one process), and
    /// naming the columns removes the need for any ordering at all.
    pub async fn update(
        self,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        // A re-tag moves a reference, and the move rides the very transaction
        // that moves the link: the old subject's release, the new one's claim
        // and the row write commit together or not at all, so no crash can
        // strand a count on a subject nothing points at (which would make it
        // undeletable forever).
        //
        // The two are armed on *different* conditions, which is the whole of
        // the rule ([`crate::domain::field_update::FieldUpdate::refcount`] states
        // it the same way): the counter statements only when the link actually
        // changes, but the CAS whenever this write *carries* the link — and it
        // always does, because the handler fills an omitted `subject_id` from
        // the row it read (`web::exams::questions`), so `subject` is in the
        // `SET` of every one of these updates. Arming the CAS on "changed"
        // instead would leave the re-stater through: a PATCH carrying the
        // subject its own stale snapshot held, sent while a rival's move
        // already landed, writes that stale subject straight back over the
        // winner — a revert with the counters left pointing at the move.
        let retag = (subject != self.subject).then(|| (subject.record(), self.subject.record()));
        let write = "UPDATE $id SET subject = $subject, text = $text, points = $points,
             kind = $kind, choices = $choices, correct = $correct";
        let mut bindings = vec![
            ("id".into(), self.id.record().into_value()),
            ("subject".into(), subject.record().into_value()),
            ("text".into(), text.into_value()),
            ("points".into(), points.into_value()),
            ("kind".into(), spec.kind.into_value()),
            ("choices".into(), spec.choices.into_value()),
            ("correct".into(), spec.correct.into_value()),
        ];
        let mut statements: Vec<String> = Vec::new();
        let mut refusals: Vec<(&str, AppError)> = Vec::new();
        if let Some((next, previous)) = &retag {
            statements.push(format!(
                "UPDATE $ref_release SET {SUBJECT_QUESTION_COUNT_FIELD} = \
                 math::max([({SUBJECT_QUESTION_COUNT_FIELD} ?? 0) - 1, 0])"
            ));
            statements.push(format!(
                "LET $seat = (UPDATE $ref_claim SET {SUBJECT_QUESTION_COUNT_FIELD} = \
                 ({SUBJECT_QUESTION_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id)"
            ));
            statements.push(format!(
                "IF array::len($seat) = 0 {{ THROW '{SUBJECT_MARK}' }}"
            ));
            bindings.push(("ref_claim".into(), next.clone().into_value()));
            bindings.push(("ref_release".into(), previous.clone().into_value()));
            refusals.push((SUBJECT_MARK, dead_subject()));
        }
        // The CAS: the row write matches only while the question still sits on
        // the subject this handler read. A genuine no-op re-state passes it
        // trivially (the row holds exactly what is expected); a stale one —
        // whether it moves the link or restates it — matches nothing, aborts
        // the whole transaction, and so claims nothing and answers 409.
        bindings.push(("ref_expected".into(), self.subject.record().into_value()));
        statements.push(format!(
            "LET $row = ({write} WHERE subject = $ref_expected RETURN AFTER)"
        ));
        statements.push(format!(
            "IF array::len($row) = 0 {{ \
             LET $live = (UPDATE $id WHERE subject != $ref_expected RETURN VALUE id); \
             IF array::len($live) = 0 {{ THROW '{ROW_MARK}' }} \
             ELSE {{ THROW '{STALE_MARK}' }} }}"
        ));
        statements.push("RETURN $row".into());
        refusals.push((
            STALE_MARK,
            AppError::Conflict(
                "the subject this question was read on changed since; re-read and retry",
            ),
        ));
        // The row is gone: the same 404 an empty write answers with, so the
        // deleted-row case is unchanged.
        refusals.push((ROW_MARK, AppError::NotFound));
        let statements = format!("{};", statements.join("; "));
        // One counter write in flight at a time — only a move writes one.
        let _guard = match &retag {
            Some(_) => Some(cap::counter_lock().await),
            None => None,
        };
        let mut result =
            ExamAttempt::write_unfrozen_with(&self.exam, &statements, bindings, refusals, db)
                .await?;
        // Read off the trailing `RETURN` rather than a fixed slot: a re-tag
        // arms three more statements than a plain PATCH does (and an `IF`
        // block is one slot whether or not it is taken).
        let slot = result.num_statements().saturating_sub(2);
        result
            .take::<Vec<ExamQuestion>>(slot)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
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
    /// Refused once the exam has an attempt, in the same transaction as the
    /// delete — and the cascade now shares that transaction too, so a failure
    /// mid-way can no longer strand answers whose question survived.
    pub async fn delete(self, db: &Database) -> Result<ExamQuestion, AppError> {
        let mut result = ExamAttempt::write_unfrozen(
            &self.exam,
            &format!(
                "DELETE exam_answer WHERE question = $q;
                 DELETE question_image WHERE question = $q;
                 LET $gone = (DELETE $q RETURN BEFORE);
                 FOR $sub IN ($gone.subject ?? []) {{
                     UPDATE $sub SET {SUBJECT_QUESTION_COUNT_FIELD} =
                         math::max([({SUBJECT_QUESTION_COUNT_FIELD} ?? 0) - 1, 0])
                 }};
                 RETURN $gone;"
            ),
            vec![("q".into(), self.id.record().into_value())],
            db,
        )
        .await?;
        // The subject's reference is given back inside this same transaction,
        // driven off what the delete actually removed — a question that wasn't
        // there decrements nothing. Read through the trailing `RETURN` rather
        // than a hand-counted slot, so inserting a cascade statement above can
        // never turn a delete into a 404 (see [`crate::domain::exam::Exam`]).
        let slot = result.num_statements().saturating_sub(2);
        result
            .take::<Vec<ExamQuestion>>(slot)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
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

    /// The subject reference counter, which is what the subject's delete guard
    /// reads: every one of these asserts *stored* state, because a claim that
    /// outlives the row it accounts for makes its subject undeletable forever.
    mod counters {
        use super::*;
        use crate::database::init_mem;
        use crate::domain::course::CourseId;
        use crate::domain::exam::{
            Exam, ExamAttemptLimit, ExamDescription, ExamKind, ExamMode, ExamSchedule, ExamTitle,
        };
        use crate::domain::settings::Settings;
        use crate::domain::subject::{Subject, SubjectDescription, SubjectName};
        use crate::domain::user::UserId;

        async fn an_exam(db: &Database) -> Exam {
            let kinds = Settings::defaults().get_exam_kinds().to_vec();
            Exam::create(
                &UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA"),
                &CourseId::from_key("01TESTCOURSEAAAAAAAAAAAAAA"),
                ExamTitle::try_new("practice").unwrap(),
                ExamDescription::try_new("").unwrap(),
                ExamKind::try_new("quiz", &kinds).unwrap(),
                ExamSchedule::try_new(Some(ExamMode::try_new("open").unwrap()), None, None, None)
                    .unwrap(),
                ExamAttemptLimit::try_new(1).unwrap(),
                true,
                false,
                false,
                db,
            )
            .await
            .unwrap()
        }

        async fn a_subject(db: &Database) -> Subject {
            Subject::create(
                &CourseId::generate(),
                SubjectName::try_new("topic").unwrap(),
                SubjectDescription::try_new("").unwrap(),
                db,
            )
            .await
            .unwrap()
        }

        /// The editable payload, fresh per call (each one is consumed).
        fn body() -> (QuestionText, QuestionPoints, QuestionSpec) {
            (
                QuestionText::try_new("3 + 3?").unwrap(),
                QuestionPoints::try_new(5).unwrap(),
                QuestionSpec::try_new(kind("text"), None, None, &[]).unwrap(),
            )
        }

        async fn a_question(exam: &Exam, on: &SubjectId, db: &Database) -> ExamQuestion {
            let (text, points, spec) = body();
            ExamQuestion::create(exam.get_id(), on.clone(), text, points, spec, db)
                .await
                .unwrap()
        }

        async fn moved(
            question: ExamQuestion,
            to: &SubjectId,
            db: &Database,
        ) -> Result<ExamQuestion, AppError> {
            let (text, points, spec) = body();
            question.update(to.clone(), text, points, spec, db).await
        }

        /// The stored `exam_question_count` on one subject, absent = zero.
        async fn count_on(subject: &SubjectId, db: &Database) -> i64 {
            let mut result = db
                .query(format!(
                    "SELECT VALUE ({SUBJECT_QUESTION_COUNT_FIELD} ?? 0) FROM $sub"
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

        async fn rows(sql: &str, db: &Database) -> usize {
            let mut result = db.query(sql).await.unwrap().check().unwrap();
            result.take::<Vec<RecordId>>(0).unwrap().len()
        }

        /// The freeze outranks the counter move, on both paths — and because
        /// the claim now rides the refused transaction, "outranks" has to mean
        /// the counters read as if nothing ran.
        #[tokio::test]
        async fn a_frozen_exam_leaves_the_subject_counters_untouched() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let (from, to) = (a_subject(&db).await, a_subject(&db).await);
            let (from, to) = (from.get_id().clone(), to.get_id().clone());
            let question = a_question(&exam, &from, &db).await;
            assert_eq!(count_on(&from, &db).await, 1);

            // A real user row: starting a sitting moves that student's badge
            // counter in the same transaction, and an `UPDATE` has nothing to
            // write to without one.
            let hash = crate::domain::user::Password::try_new("secret1")
                .unwrap()
                .hash_async()
                .await
                .unwrap();
            let student = crate::domain::user::User::create(
                crate::domain::user::Username::try_new("ogrenci").unwrap(),
                hash,
                &db,
            )
            .await
            .unwrap();
            ExamAttempt::start(&exam, student.get_id(), &db)
                .await
                .unwrap();

            let (text, points, spec) = body();
            let error = ExamQuestion::create(exam.get_id(), to.clone(), text, points, spec, &db)
                .await
                .expect_err("a started exam takes no new questions");
            assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
            assert_eq!(count_on(&to, &db).await, 0, "the refused claim rolled back");

            let error = moved(question, &to, &db)
                .await
                .expect_err("a started exam takes no re-tag either");
            assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
            assert_eq!(count_on(&from, &db).await, 1, "the release rolled back too");
            assert_eq!(count_on(&to, &db).await, 0);
        }

        /// The create-path invariant: a refused create writes neither the row
        /// nor a count — least of all on a subject it would have to invent.
        #[tokio::test]
        async fn a_create_on_a_dead_subject_writes_neither_row_nor_count() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let subject = a_subject(&db).await;
            let id = subject.get_id().clone();
            subject.delete(&db).await.unwrap();

            let (text, points, spec) = body();
            let error = ExamQuestion::create(exam.get_id(), id, text, points, spec, &db)
                .await
                .expect_err("a subject that is gone must not be taggable");
            assert!(error.to_string().contains("subject does not exist"));
            assert_eq!(
                rows("SELECT VALUE id FROM exam_question", &db).await,
                0,
                "a refused create may write no row"
            );
            assert_eq!(
                rows("SELECT VALUE id FROM subject", &db).await,
                0,
                "…and least of all a count on a subject it just brought back"
            );
        }

        #[tokio::test]
        async fn a_subject_move_moves_the_count() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let (from, to) = (a_subject(&db).await, a_subject(&db).await);
            let (from, to) = (from.get_id().clone(), to.get_id().clone());
            let question = a_question(&exam, &from, &db).await;

            let after = moved(question, &to, &db).await.unwrap();
            assert_eq!(after.get_subject(), &to);
            assert_eq!(count_on(&from, &db).await, 0, "the old subject is free");
            assert_eq!(count_on(&to, &db).await, 1, "the new one is not");
        }

        #[tokio::test]
        async fn a_move_to_a_dead_subject_leaves_everything_untouched() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let from = a_subject(&db).await.get_id().clone();
            let dead = a_subject(&db).await;
            let gone = dead.get_id().clone();
            dead.delete(&db).await.unwrap();
            let question = a_question(&exam, &from, &db).await;

            let error = moved(question.clone(), &gone, &db)
                .await
                .expect_err("a subject that is gone must not be taggable");
            assert!(error.to_string().contains("subject does not exist"));
            let stored = ExamQuestion::read(question.get_id(), &db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.get_subject(), &from, "the link never moved");
            assert_eq!(
                count_on(&from, &db).await,
                1,
                "the release rolled back with the claim"
            );
            assert_eq!(rows("SELECT VALUE id FROM subject", &db).await, 1);
        }

        /// The double-claim guard. Both movers compute their claim and release
        /// from the row as *they* read it, so two PATCHes re-tagging the same
        /// question both release the old subject and both claim their own
        /// target — two counts for one link, and the loser's target is
        /// undeletable forever. The second call here runs on the struct read
        /// before the first one landed: it must be refused outright, and the
        /// counts must read as if it never ran.
        ///
        /// Drop `AND subject = $ref_expected` from the row write in
        /// [`ExamQuestion::update`] and this goes red on the very first
        /// assertion — the stale mover is happily applied.
        #[tokio::test]
        async fn a_stale_mover_is_refused_and_claims_nothing() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let from = a_subject(&db).await.get_id().clone();
            let to = a_subject(&db).await.get_id().clone();
            let other = a_subject(&db).await.get_id().clone();
            let question = a_question(&exam, &from, &db).await;
            let stale = question.clone();

            moved(question, &to, &db).await.unwrap();
            let error = moved(stale.clone(), &other, &db)
                .await
                .expect_err("a mover that read a subject it no longer holds must be refused");
            assert!(
                matches!(error, AppError::Conflict(_)),
                "a lost CAS is a conflict, not a 404 or a 500: {error:?}"
            );

            let stored = ExamQuestion::read(stale.get_id(), &db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.get_subject(), &to, "the winner's link");
            assert_eq!(count_on(&from, &db).await, 0, "released once, not twice");
            assert_eq!(count_on(&to, &db).await, 1, "claimed once");
            assert_eq!(count_on(&other, &db).await, 0, "never claimed");
        }

        /// The other half of the same race, and the one arming on "the subject
        /// changed" lets through: this PATCH carries the subject its snapshot
        /// already held, so it moves no counter — but a rival's move landed
        /// first, and writing that stale subject back would revert the winner
        /// while both counters still describe the move. The handler fills an
        /// omitted `subject_id` from the row it read, so this is also every
        /// text-only PATCH sent from a stale snapshot.
        ///
        /// Arm the CAS on the re-tag instead of on the write and this goes red
        /// on the first assertion — the re-state lands 200.
        #[tokio::test]
        async fn a_stale_re_stater_is_refused_and_reverts_nothing() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let from = a_subject(&db).await.get_id().clone();
            let to = a_subject(&db).await.get_id().clone();
            let question = a_question(&exam, &from, &db).await;
            let stale = question.clone();

            moved(question, &to, &db).await.unwrap();
            let error = moved(stale.clone(), &from, &db)
                .await
                .expect_err("re-stating a subject a rival moved off must be refused");
            assert!(
                matches!(error, AppError::Conflict(_)),
                "a lost CAS is a conflict, not a silent revert: {error:?}"
            );

            let stored = ExamQuestion::read(stale.get_id(), &db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.get_subject(), &to, "the winner's link stands");
            assert_eq!(
                count_on(&from, &db).await,
                0,
                "released once, and stayed so"
            );
            assert_eq!(count_on(&to, &db).await, 1, "claimed once, and stayed so");
        }

        /// …and the price of arming the CAS on every write is nothing: a PATCH
        /// re-stating the subject the row really holds passes it trivially.
        #[tokio::test]
        async fn a_no_op_re_state_still_lands() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let on = a_subject(&db).await.get_id().clone();
            let question = a_question(&exam, &on, &db).await;

            let (_, points, spec) = body();
            let after = question
                .update(
                    on.clone(),
                    QuestionText::try_new("4 + 4?").unwrap(),
                    points,
                    spec,
                    &db,
                )
                .await
                .expect("re-stating the subject the row holds is not a conflict");
            assert_eq!(after.get_text().as_str(), "4 + 4?");
            assert_eq!(after.get_subject(), &on);
            assert_eq!(count_on(&on, &db).await, 1, "no counter moved");
        }

        /// A re-tag of a question whose row is gone is still the 404 it was,
        /// not the stale-mover 409 — the two empty-`$row` cases stay apart.
        #[tokio::test]
        async fn a_move_of_a_deleted_question_is_still_a_404() {
            let db = init_mem().await.unwrap();
            let exam = an_exam(&db).await;
            let from = a_subject(&db).await.get_id().clone();
            let to = a_subject(&db).await.get_id().clone();
            let question = a_question(&exam, &from, &db).await;
            question.clone().delete(&db).await.unwrap();

            let error = moved(question, &to, &db)
                .await
                .expect_err("a deleted question cannot be re-tagged");
            assert!(matches!(error, AppError::NotFound), "{error:?}");
            assert_eq!(count_on(&to, &db).await, 0, "and nothing was claimed");
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
