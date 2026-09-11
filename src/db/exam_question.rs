//! The `exam_question` table: row reads and listings, the answer-key join
//! behind review hiding, and the field-scoped writes that tie the freeze gate
//! and the subject's reference counter into their own transaction. The
//! workflows and the freeze pre-flight live in
//! [`crate::service::exam_question`].

use std::collections::HashSet;

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::SUBJECT_QUESTION_COUNT_FIELD;
use crate::database::Database;
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    ExamQuestion, ExamQuestionId, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};

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

pub async fn create(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    insert(db, exam, subject, text, points, spec, None).await
}

/// Like [`create`], but records the bank template this question was
/// instantiated from (`POST …/questions/from-bank/{bid}`).
pub async fn create_from_bank(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    insert(db, exam, subject, text, points, spec, Some(source)).await
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    from_bank: Option<BankQuestionId>,
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
    let mut result = crate::db::exam_attempt::write_unfrozen_with(
        db,
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

pub async fn read(db: &Database, id: &ExamQuestionId) -> Result<Option<ExamQuestion>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// The exam's questions in presentation order (ULID ids sort by creation).
pub async fn list_for_exam(
    db: &Database,
    exam: &ExamId,
    limit: Option<i64>,
    offset: i64,
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
/// bank ([`link_banked_as`]). A question authored by hand in exam A
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
    db: &Database,
    exam: &ExamId,
    live: &[ExamId],
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
/// `from_bank`/`banked_as`, which [`link_banked_as`] writes from a
/// *different* request: re-stating this snapshot's copy of them would
/// revert a to-bank save that landed in between. That is exactly what the
/// caller's `EXAM_LOCK.write()` used to order (inside one process), and
/// naming the columns removes the need for any ordering at all.
pub async fn update(
    db: &Database,
    question: ExamQuestion,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    // A re-tag moves a reference, and the move rides the very transaction
    // that moves the link: the old subject's release, the new one's claim
    // and the row write commit together or not at all, so no crash can
    // strand a count on a subject nothing points at (which would make it
    // undeletable forever).
    //
    // The two are armed on *different* conditions, which is the whole of
    // the rule ([`crate::db::field_update::FieldUpdate::refcount`] states
    // it the same way): the counter statements only when the link actually
    // changes, but the CAS whenever this write *carries* the link — and it
    // always does, because the handler fills an omitted `subject_id` from
    // the row it read (`web::exams::questions`), so `subject` is in the
    // `SET` of every one of these updates. Arming the CAS on "changed"
    // instead would leave the re-stater through: a PATCH carrying the
    // subject its own stale snapshot held, sent while a rival's move
    // already landed, writes that stale subject straight back over the
    // winner — a revert with the counters left pointing at the move.
    let retag =
        (subject != question.subject).then(|| (subject.record(), question.subject.record()));
    let write = "UPDATE $id SET subject = $subject, text = $text, points = $points,
         kind = $kind, choices = $choices, correct = $correct";
    let mut bindings = vec![
        ("id".into(), question.id.record().into_value()),
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
    bindings.push((
        "ref_expected".into(),
        question.subject.record().into_value(),
    ));
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
    let mut result = crate::db::exam_attempt::write_unfrozen_with(
        db,
        &question.exam,
        &statements,
        bindings,
        refusals,
    )
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
/// `from_bank` [`create_from_bank`] writes, and deliberately a
/// *different* column: a question inserted from the bank has not been saved
/// to it, and one field for both would make the client claim it was.
/// Overwrites any earlier link: repeat saves mint a new template and the
/// newest one wins.
///
/// Field-scoped write, unlike [`update`]: the caller awaits a bank
/// insert plus the whole blob-copy loop between reading this row and
/// linking it, so the row it holds is long stale by now — a whole-row save
/// would silently revert whatever landed in that window. (The caller takes
/// `EXAM_LOCK.read()` around this call, which is what keeps
/// [`update`]'s whole-row save from clobbering the link in the other
/// direction; the lease guards the ordering, not the staleness.)
pub async fn link_banked_as(
    db: &Database,
    question: ExamQuestion,
    template: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    let mut result = db
        .query("UPDATE $id SET banked_as = $bank RETURN AFTER")
        .bind(("id", question.id.record()))
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
pub async fn delete(db: &Database, question: ExamQuestion) -> Result<ExamQuestion, AppError> {
    let mut result = crate::db::exam_attempt::write_unfrozen(
        db,
        &question.exam,
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
        vec![("q".into(), question.id.record().into_value())],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::QuestionKind;

    fn kind(value: &str) -> QuestionKind {
        QuestionKind::try_new(value).unwrap()
    }

    /// The subject reference counter, which is what the subject's delete guard
    /// reads: every one of these asserts *stored* state, because a claim that
    /// outlives the row it accounts for makes its subject undeletable forever.
    mod counters {
        use super::*;
        use crate::database::init_mem;
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
                &crate::db::course::a_test_course(db).await,
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
                &crate::db::course::a_test_course(db).await,
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
            create(db, exam.get_id(), on.clone(), text, points, spec)
                .await
                .unwrap()
        }

        async fn moved(
            question: ExamQuestion,
            to: &SubjectId,
            db: &Database,
        ) -> Result<ExamQuestion, AppError> {
            let (text, points, spec) = body();
            update(db, question, to.clone(), text, points, spec).await
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
            crate::service::exam_attempt::start(&db, &exam, student.get_id())
                .await
                .unwrap();

            let (text, points, spec) = body();
            let error = create(&db, exam.get_id(), to.clone(), text, points, spec)
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
            let error = create(&db, exam.get_id(), id, text, points, spec)
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
            let stored = read(&db, question.get_id()).await.unwrap().unwrap();
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
        /// [`update`] and this goes red on the very first
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

            let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
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

            let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
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
            let after = update(
                &db,
                question,
                on.clone(),
                QuestionText::try_new("4 + 4?").unwrap(),
                points,
                spec,
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
            delete(&db, question.clone()).await.unwrap();

            let error = moved(question, &to, &db)
                .await
                .expect_err("a deleted question cannot be re-tagged");
            assert!(matches!(error, AppError::NotFound), "{error:?}");
            assert_eq!(count_on(&to, &db).await, 0, "and nothing was claimed");
        }
    }
}
