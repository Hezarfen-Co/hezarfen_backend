//! The `exam` table: row reads and listings, the gated create that refuses a
//! course that is gone, the compare-and-set every PATCH writes through, and
//! the cascading delete. The PATCH re-derive lives in [`crate::service::exam`].

use sqlx::PgConnection;

use crate::database::{Database, foreign_key_violation, tx_with_retry};
use crate::domain::class_course::ClassCourseId;
use crate::domain::course::CourseId;
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode,
    ExamSchedule, ExamTitle, redraft_error,
};
use crate::domain::term::TermId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' create(field, field, ..) shape"
)]
pub async fn create(
    db: &Database,
    creator: &UserId,
    class_course: &ClassCourseId,
    term: &TermId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    schedule: ExamSchedule,
    max_attempts: ExamAttemptLimit,
    allow_rejoin: bool,
    allow_review: bool,
    draft: bool,
) -> Result<Exam, AppError> {
    // A missing parent is refused by the real foreign keys: an instance, a
    // term or a creator that is gone is `SQLSTATE 23503`, and this call
    // site's parent-gone answer is the same `NotFound` the old
    // existence-proof touch answered with.
    //
    // The term is *claimed* in the same statement as the row (`exam_count`
    // +1, a conditional write on the term row), the shape the old course
    // create used against `term.course_count`: the count is what the term's
    // own delete guard reads, and claim-and-insert in one CTE means no crash
    // can leave one half without the other.
    //
    // The owner's `exam_audience` row is written in the same transaction,
    // *always*: the audience is what every listing reads through, so an exam
    // with no audience row would be invisible to its own instance. A shared
    // exam is extra audience rows on this one exam (D2), never a second
    // shape.
    let id = ExamId::generate();
    let creator = *creator;
    let class_course = class_course.clone();
    // `TermId` is `Copy`: one copy here, and the per-attempt closure below
    // re-yields values rather than clones.
    let term = *term;
    // The binds are hoisted to owned values so the closure below only has to
    // clone them per attempt: the write itself is a named future, for the
    // reason [`insert_in`] gives.
    let title_text = title.as_str().to_string();
    let description_text = description.as_str().to_string();
    let kind_text = kind.as_str().to_string();
    let mode_text = schedule.mode.as_ref().map(|mode| mode.as_str().to_string());
    let starts_at = schedule.starts_at.map(|at| at.as_millis());
    let ends_at = schedule.ends_at.map(|at| at.as_millis());
    let duration_ms = schedule.duration_ms.map(|duration| duration.as_millis());
    let max_attempts = max_attempts.as_i64();
    let created = tx_with_retry(db, false, async move |tx| {
        insert_in(
            tx,
            id.clone(),
            creator,
            class_course.clone(),
            term,
            title_text.clone(),
            description_text.clone(),
            kind_text.clone(),
            mode_text.clone(),
            starts_at,
            ends_at,
            duration_ms,
            max_attempts,
            allow_rejoin,
            allow_review,
            draft,
        )
        .await
    })
    .await?;
    Ok(created)
}

/// The claim, the row and its audience row, on one connection — [`create`]'s
/// body as a *named* future.
///
/// Named, and taking owned values, because the anonymous `async move |tx|`
/// closure does not work here: with this many binds, rustc cannot prove the
/// closure's future is `Send` under [`tx_with_retry`]'s
/// `AsyncFnMut(&mut PgConnection)` higher-ranked bound, so every axum handler
/// that awaits [`create`] fails with "implementation of `Send` is not general
/// enough" pointing at its `routes!` entry. A named `async fn` has a concrete
/// future type rustc can check, exactly as
/// [`crate::db::answer_image::upsert_in`] does for the same reason.
///
/// Everything it decides is the closure's own: a claim that matched no term
/// row is [`crate::domain::term::gone_error`], any other foreign key is the
/// parent-gone `NotFound`, and the audience row is written in the same
/// transaction as the row it names.
#[expect(
    clippy::too_many_arguments,
    reason = "the create's own field list, spelled once for the named future"
)]
pub(crate) async fn insert_in(
    tx: &mut PgConnection,
    id: ExamId,
    creator: UserId,
    class_course: ClassCourseId,
    term: TermId,
    title: String,
    description: String,
    kind: String,
    mode: Option<String>,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
    duration_ms: Option<i64>,
    max_attempts: i64,
    allow_rejoin: bool,
    allow_review: bool,
    draft: bool,
) -> Result<Exam, AppError> {
    let written = sqlx::query_as!(
        Exam,
        r#"WITH room AS (
                   UPDATE term SET exam_count = exam_count + 1
                    WHERE id = $3
                    RETURNING 1)
               INSERT INTO exam (id, creator, class_course, term, title, description, kind, mode,
                                 starts_at, ends_at, duration_ms, max_attempts,
                                 allow_rejoin, allow_review, draft)
               SELECT $1, $2, $4, $3, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
                WHERE EXISTS (SELECT 1 FROM room)
               RETURNING id AS "id: ExamId", creator AS "creator: UserId",
                         class_course AS "class_course: ClassCourseId",
                         term AS "term: TermId", title AS "title: ExamTitle",
                         description AS "description: ExamDescription",
                         kind AS "kind: ExamKind", mode AS "mode: ExamMode",
                         starts_at AS "starts_at: Timestamp",
                         ends_at AS "ends_at: Timestamp",
                         duration_ms AS "duration_ms: ExamDuration",
                         max_attempts AS "max_attempts: ExamAttemptLimit",
                         allow_rejoin, allow_review, draft"#,
        id.uuid(),
        creator.uuid(),
        term.uuid(),
        class_course.uuid(),
        title.as_str(),
        description.as_str(),
        kind.as_str(),
        mode.as_deref(),
        starts_at,
        ends_at,
        duration_ms,
        max_attempts,
        allow_rejoin,
        allow_review,
        draft,
    )
    .fetch_optional(&mut *tx)
    .await;
    let exam = match written {
        Ok(Some(exam)) => exam,
        // The conditional claim matched no term row: the term is gone.
        Ok(None) => return Err(crate::domain::term::gone_error()),
        Err(err) if foreign_key_violation(&err) => return Err(AppError::NotFound),
        Err(err) => return Err(err.into()),
    };
    sqlx::query!(
        r#"INSERT INTO exam_audience (exam, class_course) VALUES ($1, $2)"#,
        exam.id.uuid(),
        exam.class_course.uuid(),
    )
    .execute(&mut *tx)
    .await?;
    Ok(exam)
}

pub async fn read(db: &Database, id: &ExamId) -> Result<Option<Exam>, AppError> {
    Ok(sqlx::query_as!(
        Exam,
        r#"SELECT id AS "id: ExamId", creator AS "creator: UserId",
                  class_course AS "class_course: ClassCourseId", term AS "term: TermId",
                  title AS "title: ExamTitle", description AS "description: ExamDescription",
                  kind AS "kind: ExamKind", mode AS "mode: ExamMode",
                  starts_at AS "starts_at: Timestamp",
                  ends_at AS "ends_at: Timestamp",
                  duration_ms AS "duration_ms: ExamDuration",
                  max_attempts AS "max_attempts: ExamAttemptLimit",
                  allow_rejoin, allow_review, draft
           FROM exam WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?)
}

/// The exams, newest first — `ORDER BY id` on a UUIDv7 column is creation
/// order, the sort the old record ids gave for free.
pub async fn list_all(db: &Database) -> Result<Vec<Exam>, AppError> {
    Ok(sqlx::query_as!(
        Exam,
        r#"SELECT id AS "id: ExamId", creator AS "creator: UserId",
                  class_course AS "class_course: ClassCourseId", term AS "term: TermId",
                  title AS "title: ExamTitle", description AS "description: ExamDescription",
                  kind AS "kind: ExamKind", mode AS "mode: ExamMode",
                  starts_at AS "starts_at: Timestamp",
                  ends_at AS "ends_at: Timestamp",
                  duration_ms AS "duration_ms: ExamDuration",
                  max_attempts AS "max_attempts: ExamAttemptLimit",
                  allow_rejoin, allow_review, draft
           FROM exam ORDER BY id DESC"#,
    )
    .fetch_all(db)
    .await?)
}

/// The exams of one instance, newest first.
///
/// It reads through `exam_audience` rather than off `exam.class_course`: the
/// owner's row is always there (the create writes it), so the two agree for
/// every exam this instance owns, and the join is what lets a shared exam be
/// one exam addressed to several instances (D2) without a second read path.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<Vec<Exam>, AppError> {
    Ok(sqlx::query_as!(
        Exam,
        r#"SELECT DISTINCT ON (e.id)
                  e.id AS "id: ExamId", e.creator AS "creator: UserId",
                  e.class_course AS "class_course: ClassCourseId",
                  e.term AS "term: TermId", e.title AS "title: ExamTitle",
                  e.description AS "description: ExamDescription",
                  e.kind AS "kind: ExamKind", e.mode AS "mode: ExamMode",
                  e.starts_at AS "starts_at: Timestamp",
                  e.ends_at AS "ends_at: Timestamp",
                  e.duration_ms AS "duration_ms: ExamDuration",
                  e.max_attempts AS "max_attempts: ExamAttemptLimit",
                  e.allow_rejoin, e.allow_review, e.draft
           FROM exam e
           JOIN exam_audience a ON a.exam = e.id
           WHERE a.class_course = $1
           ORDER BY e.id DESC"#,
        class_course.uuid(),
    )
    .fetch_all(db)
    .await?)
}

/// Every exam of every instance in `instances` (one query) — the report card and
/// marks reports' cross-instance read, and the list behind a caller's visible
/// courses.
///
/// `DISTINCT ON (e.id)` because an exam may be addressed to several instances
/// at once (a shared exam): the filters ask "is this exam visible here", not
/// "how many audiences does it have", so each exam is returned once with its
/// own owner instance.
pub async fn list_for_class_course_courses(
    db: &Database,
    instances: &[ClassCourseId],
) -> Result<Vec<Exam>, AppError> {
    if instances.is_empty() {
        return Ok(Vec::new());
    }
    let instances = instances
        .iter()
        .map(ClassCourseId::uuid)
        .collect::<Vec<_>>();
    Ok(sqlx::query_as!(
        Exam,
        r#"SELECT DISTINCT ON (e.id)
                  e.id AS "id: ExamId", e.creator AS "creator: UserId",
                  e.class_course AS "class_course: ClassCourseId",
                  e.term AS "term: TermId", e.title AS "title: ExamTitle",
                  e.description AS "description: ExamDescription",
                  e.kind AS "kind: ExamKind", e.mode AS "mode: ExamMode",
                  e.starts_at AS "starts_at: Timestamp",
                  e.ends_at AS "ends_at: Timestamp",
                  e.duration_ms AS "duration_ms: ExamDuration",
                  e.max_attempts AS "max_attempts: ExamAttemptLimit",
                  e.allow_rejoin, e.allow_review, e.draft
           FROM exam e
           JOIN exam_audience a ON a.exam = e.id
           WHERE a.class_course = ANY($1)
           ORDER BY e.id DESC"#,
        &instances,
    )
    .fetch_all(db)
    .await?)
}

/// Every exam of every *catalog* course in `courses` (one query) — the
/// catalog as one user sees it, across the instances those courses are taught
/// in.
pub async fn list_for_courses(db: &Database, courses: &[CourseId]) -> Result<Vec<Exam>, AppError> {
    if courses.is_empty() {
        return Ok(Vec::new());
    }
    let courses = courses.iter().map(CourseId::uuid).collect::<Vec<_>>();
    Ok(sqlx::query_as!(
        Exam,
        r#"SELECT DISTINCT ON (e.id)
                  e.id AS "id: ExamId", e.creator AS "creator: UserId",
                  e.class_course AS "class_course: ClassCourseId",
                  e.term AS "term: TermId", e.title AS "title: ExamTitle",
                  e.description AS "description: ExamDescription",
                  e.kind AS "kind: ExamKind", e.mode AS "mode: ExamMode",
                  e.starts_at AS "starts_at: Timestamp",
                  e.ends_at AS "ends_at: Timestamp",
                  e.duration_ms AS "duration_ms: ExamDuration",
                  e.max_attempts AS "max_attempts: ExamAttemptLimit",
                  e.allow_rejoin, e.allow_review, e.draft
           FROM exam e
           JOIN exam_audience a ON a.exam = e.id
           JOIN class_course cc ON cc.id = a.class_course
           WHERE cc.course = ANY($1)
           ORDER BY e.id DESC"#,
        &courses,
    )
    .fetch_all(db)
    .await?)
}

/// The exam's mark counter, read on its own — the one column no struct
/// carries and no whole-row rewrite touches. The kind-change gate reads it
/// here (pre-flight) and `update_if_unchanged` pins it inside its own
/// transaction (write time).
pub async fn result_count(db: &Database, id: &ExamId) -> Result<i64, AppError> {
    let row = sqlx::query!(
        r#"SELECT result_count AS "result_count: i64" FROM exam WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_one(db)
    .await?;
    Ok(row.result_count)
}

// `course` is deliberately not updatable — moving an exam between courses
// would strand results of students not enrolled in the target course.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' update(field, field, ..) shape"
)]
/// Save the merged exam, but only while the row still reads as the
/// snapshot the caller merged over — `None` means a concurrent PATCH
/// landed in between and nothing was written: re-read, re-merge, retry.
///
/// Every column this write replaces is in the guard, which is what makes
/// the whole-row save safe without a lock held across the handler's read:
/// the compare-and-set refuses precisely when that save would have
/// reverted somebody. `course`/`creator` are not editable and ride along
/// unchanged. Same shape as [`crate::db::settings::save_if_unchanged`].
///
/// The mark counter is pinned too, read *inside* this transaction rather
/// than carried on the snapshot: a grade increments it, so pinning it is
/// what makes "this exam had no marks" — the gate the handler refuses a
/// kind change on — true at *write* time and not merely at read time. A
/// mark landing between the read and the guarded write makes the UPDATE
/// re-check its `WHERE` against the new row version, find the counter
/// moved, and refuse the save — the same answer the old pinned snapshot
/// gave, without a stale snapshot able to pin a counter it cannot see.
///
/// The re-draft gate rides in the same transaction, as before: re-drafting
/// hides an exam, so it must be refused while any sitting or mark exists,
/// and the check has to see the write's own moment, not the handler's.
pub async fn update_if_unchanged(
    db: &Database,
    mut expected: Exam,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    schedule: ExamSchedule,
    max_attempts: ExamAttemptLimit,
    allow_rejoin: bool,
    allow_review: bool,
    draft: bool,
) -> Result<Option<Exam>, AppError> {
    let redraft = draft && !expected.draft;
    let was = (
        expected.title.clone(),
        expected.description.clone(),
        expected.kind.clone(),
        expected.mode.clone(),
        expected.starts_at,
        expected.ends_at,
        expected.duration_ms,
        expected.max_attempts,
        expected.allow_rejoin,
        expected.allow_review,
        expected.draft,
    );
    expected.title = title;
    expected.description = description;
    expected.kind = kind;
    expected.mode = schedule.mode;
    expected.starts_at = schedule.starts_at;
    expected.ends_at = schedule.ends_at;
    expected.duration_ms = schedule.duration_ms;
    expected.max_attempts = max_attempts;
    expected.allow_rejoin = allow_rejoin;
    expected.allow_review = allow_review;
    expected.draft = draft;
    let saved = tx_with_retry(db, false, async move |conn| {
        if redraft {
            // `exam_redraft`: the same refusal the handler's pre-flight
            // answers, re-made at write time. A sitting or a mark existing
            // hides nothing.
            let gates = sqlx::query!(
                r#"SELECT EXISTS(SELECT 1 FROM exam_attempt WHERE exam = $1) AS sat,
                          EXISTS(SELECT 1 FROM exam_result WHERE exam = $1) AS graded"#,
                expected.id.uuid(),
            )
            .fetch_one(&mut *conn)
            .await?;
            if gates.sat.unwrap_or(false) || gates.graded.unwrap_or(false) {
                return Err(redraft_error());
            }
        }
        let was_results = sqlx::query!(
            r#"SELECT result_count AS "result_count: i64" FROM exam WHERE id = $1"#,
            expected.id.uuid(),
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.result_count)
        .unwrap_or(0);
        let written = sqlx::query_as!(
            Exam,
            r#"UPDATE exam SET title = $2, description = $3, kind = $4, mode = $5,
                                starts_at = $6, ends_at = $7, duration_ms = $8,
                                max_attempts = $9, allow_rejoin = $10, allow_review = $11,
                                draft = $12
               WHERE id = $1
                 AND title = $13 AND description = $14 AND kind = $15
                 AND mode IS NOT DISTINCT FROM $16
                 AND starts_at IS NOT DISTINCT FROM $17
                 AND ends_at IS NOT DISTINCT FROM $18
                 AND duration_ms IS NOT DISTINCT FROM $19
                 AND max_attempts = $20 AND allow_rejoin = $21 AND allow_review = $22
                 AND draft = $23
                 AND result_count = $24
               RETURNING id AS "id: ExamId", creator AS "creator: UserId",
                         class_course AS "class_course: ClassCourseId",
                         term AS "term: TermId", title AS "title: ExamTitle",
                         description AS "description: ExamDescription",
                         kind AS "kind: ExamKind", mode AS "mode: ExamMode",
                         starts_at AS "starts_at: Timestamp",
                         ends_at AS "ends_at: Timestamp",
                         duration_ms AS "duration_ms: ExamDuration",
                         max_attempts AS "max_attempts: ExamAttemptLimit",
                         allow_rejoin, allow_review, draft"#,
            expected.id.uuid(),
            expected.title.as_str(),
            expected.description.as_str(),
            expected.kind.as_str(),
            expected.mode.as_ref().map(ExamMode::as_str),
            expected.starts_at.map(|at| at.as_millis()),
            expected.ends_at.map(|at| at.as_millis()),
            expected.duration_ms.map(|duration| duration.as_millis()),
            expected.max_attempts.as_i64(),
            expected.allow_rejoin,
            expected.allow_review,
            expected.draft,
            was.0.as_str(),
            was.1.as_str(),
            was.2.as_str(),
            was.3.as_ref().map(ExamMode::as_str),
            was.4.map(|at| at.as_millis()),
            was.5.map(|at| at.as_millis()),
            was.6.map(|duration| duration.as_millis()),
            was.7.as_i64(),
            was.8,
            was.9,
            was.10,
            was_results,
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(written)
    })
    .await?;
    Ok(saved)
}

/// What [`delete`] collects on its way through: the deleted row plus the
/// blob keys of the image rows its cascade removed — exactly whose files
/// the web layer may unlink.
#[derive(Debug)]
pub struct Deleted {
    pub exam: Exam,
    pub question_image_files: Vec<String>,
    pub answer_image_files: Vec<String>,
}

/// Delete the exam and cascade-remove its result, attempt, question,
/// answer, question-image, and answer-image rows — all in one transaction,
/// so a failure can't leave an emptied-out exam shell behind. The image
/// *blobs* are the caller's to remove — this returns the names, collected
/// inside the same transaction that removes the rows.
///
/// The transaction opens by taking the exam row `FOR UPDATE` — the store
/// replacement of the writer lease the delete used to hold. Every child
/// writer that matters locks the same row first (a sitting create's guard,
/// the freeze gate, an answer save ahead of its upsert), so a start or a
/// save either finished before the sweep (which then takes its row) or
/// finds no exam and is a `404`. Left orphaned, an attempt kept a sitting
/// on the student's lifetime counter and could mint a badge — awards are
/// add-only and never revoked — for an exam that never existed.
///
/// Bank templates saved out of this exam survive it — they are a separate,
/// reusable library — so only their `source_exam` provenance link is
/// cleared, in the same transaction, never left pointing at a dead exam.
///
/// The questions about to be cascaded each hold a reference on their
/// subject, which is what keeps that subject from being deleted under them.
/// They are given back in this same transaction, counted per subject, so
/// deleting an exam frees its subjects for deletion and nothing else does.
/// The marks give their *kind* references back the same way, and it has to
/// be the same way: counted outside the transaction, a mark deleted by a
/// concurrent `remove_result` in the gap would be released twice — once by
/// each — which on a kind another exam still uses reads as one mark too
/// few, and that is a kind wrongly free to leave the settings.
pub async fn delete(db: &Database, target: Exam) -> Result<Deleted, AppError> {
    tx_with_retry(
        db,
        true,
        async move |conn| {
            // The row lock every other exam-child writer contends on. The
            // term rides out of the same read: the term's `exam_count` is
            // given back in this transaction, and the row that names it is
            // the one being deleted.
            let locked = sqlx::query!(
                r#"SELECT term AS "term: TermId" FROM exam WHERE id = $1 FOR UPDATE"#,
                target.id.uuid(),
            )
            .fetch_optional(&mut *conn)
            .await?;
            let Some(locked) = locked else {
                return Err(AppError::NotFound);
            };
            // The blob names, collected *before* the rows go — inside the
            // lock, so an image row written after this snapshot cannot
            // strand its bytes on disk even though the row itself would be
            // refused.
            let image_files = sqlx::query!(
                r#"SELECT file FROM question_image WHERE exam = $1"#,
                target.id.uuid(),
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| row.file)
            .collect();
            let answer_image_files = sqlx::query!(
                r#"SELECT file FROM answer_image WHERE exam = $1"#,
                target.id.uuid(),
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| row.file)
            .collect();
            // The marks give their kind references back, counted per kind
            // off the results this exam still has. The ref row always
            // exists here: a mark's own claim creates it before any result
            // can land.
            sqlx::query!(
                r#"WITH kinds AS (
                       SELECT e.kind AS kind, count(*) AS n
                       FROM exam_result r JOIN exam e ON e.id = r.exam
                       WHERE r.exam = $1
                       GROUP BY e.kind)
                   UPDATE kind_ref k SET count = GREATEST(k.count - c.n, 0)
                   FROM kinds c WHERE k.name = c.kind"#,
                target.id.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(r#"DELETE FROM exam_result WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            sqlx::query!(r#"DELETE FROM exam_attempt WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            sqlx::query!(r#"DELETE FROM exam_answer WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            sqlx::query!(r#"DELETE FROM answer_image WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            sqlx::query!(r#"DELETE FROM question_image WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            // The cascaded questions give their subject references back,
            // counted per subject off the rows still present — before the
            // questions themselves go.
            sqlx::query!(
                r#"WITH subs AS (
                       SELECT subject, count(*) AS n
                       FROM exam_question WHERE exam = $1
                       GROUP BY subject)
                   UPDATE subject s SET exam_question_count = GREATEST(s.exam_question_count - c.n, 0)
                   FROM subs c WHERE s.id = c.subject"#,
                target.id.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(r#"DELETE FROM exam_question WHERE exam = $1"#, target.id.uuid())
                .execute(&mut *conn)
                .await?;
            sqlx::query!(
                r#"UPDATE bank_question SET source_exam = NULL WHERE source_exam = $1"#,
                target.id.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            // The sitting instances go with the exam: every read goes through
            // the audience join, so a row pointing at a deleted exam would be
            // both invisible and undeletable.
            sqlx::query!(
                r#"DELETE FROM exam_audience WHERE exam = $1"#,
                target.id.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            let deleted = sqlx::query_as!(
                Exam,
                r#"DELETE FROM exam WHERE id = $1
                   RETURNING id AS "id: ExamId", creator AS "creator: UserId",
                             class_course AS "class_course: ClassCourseId",
                             term AS "term: TermId", title AS "title: ExamTitle",
                             description AS "description: ExamDescription",
                             kind AS "kind: ExamKind", mode AS "mode: ExamMode",
                             starts_at AS "starts_at: Timestamp",
                             ends_at AS "ends_at: Timestamp",
                             duration_ms AS "duration_ms: ExamDuration",
                             max_attempts AS "max_attempts: ExamAttemptLimit",
                             allow_rejoin, allow_review, draft"#,
                target.id.uuid(),
            )
            .fetch_optional(&mut *conn)
            .await?;
            let Some(exam) = deleted else {
                return Err(AppError::NotFound);
            };
            // The term gets its exam reference back inside the same
            // transaction — the other half of the claim `create` makes.
            sqlx::query!(
                r#"UPDATE term SET exam_count = GREATEST(exam_count - 1, 0)
                   WHERE id = $1"#,
                locked.term.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            Ok(Deleted {
                exam,
                question_image_files: image_files,
                answer_image_files,
            })
        },
    )
    .await
}

#[cfg(test)]
use crate::domain::settings::ExamKindDef;

/// An unscheduled published exam — the minimum any test that writes a *child*
/// of an exam needs, in any module: every such write moves the exam row (see
/// [`crate::db::exam_attempt::write_unfrozen_with`]
/// and [`crate::db::exam_answer::save`]), so a minted id whose
/// row was never created is a 404 rather than a silent orphan.
#[cfg(test)]
pub(crate) async fn published_exam(db: &Database) -> Exam {
    let allowed: Vec<ExamKindDef> = crate::domain::settings::Settings::defaults()
        .get_exam_kinds()
        .to_vec();
    // The creator is a foreign key now: a real `app_user` row, minted per call.
    let creator = UserId::generate();
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, $2, 0, 'teacher')",
    )
    .bind(creator.uuid())
    .bind(format!("exam-fixture-{}", &creator.key()[30..]))
    .execute(db)
    .await
    .unwrap();
    // The exam hangs off the *instance* and carries the term it is graded in
    // (both foreign keys): both are real rows, minted per call —
    // [`crate::db::course::a_test_instance`] mints the class, the course and
    // the link, [`crate::db::term::a_test_term`] the year under the term.
    let (instance, _course) = crate::db::course::a_test_instance(db).await;
    let term = crate::db::term::a_test_term(db).await;
    create(
        db,
        &creator,
        &instance,
        &term,
        ExamTitle::try_new("midterm").unwrap(),
        ExamDescription::try_new("").unwrap(),
        ExamKind::try_new("yazili", &allowed).unwrap(),
        ExamSchedule::try_new(None, None, None, None).unwrap(),
        ExamAttemptLimit::try_new(1).unwrap(),
        true,
        false,
        false,
    )
    .await
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::published_exam as published;

    use crate::domain::exam::{ExamDuration, ExamMode};
    use crate::domain::settings::ExamKindDef;
    use crate::domain::timestamp::Timestamp;
    use crate::error::ValidationError;

    /// An exam must not outlive the course it belongs to. It is the worst of
    /// the three children `Course::delete` used to leave behind: every exam
    /// route funnels through `class_course_of`, which answers
    /// `Internal("exam references a missing course")`, so an orphan **500s
    /// forever** on `GET`/`PATCH`/`DELETE /exams/{id}` — undeletable — while
    /// [`crate::db::exam::list_all`] still hands it to every manager+ on `GET /exams`.
    ///
    /// [`create`] therefore *writes* the course row rather than reading
    /// it ([`cap::touch_and_create`]); the harness and the window it races in
    /// are documented on
    /// [`crate::db::course::assert_no_child_outlives_a_course_delete`].
    /// Mutation-tested: with the bare `db.create` this shipped with, all four
    /// rounds orphan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_exam_never_outlives_its_course() {
        fn make(course: CourseId, db: Database) -> tokio::task::JoinHandle<Result<(), AppError>> {
            tokio::spawn(async move {
                let kinds = crate::domain::settings::Settings::defaults()
                    .get_exam_kinds()
                    .to_vec();
                let creator = UserId::generate();
                sqlx::query(
                    "INSERT INTO app_user (id, username, created_at, role) \
                     VALUES ($1, $2, 0, 'teacher')",
                )
                .bind(creator.uuid())
                .bind(format!("orphan-race-{}", &creator.key()[30..]))
                .execute(&db)
                .await
                .unwrap();
                // The exam's parents: a term, and an instance of *this*
                // course. Attaching it is the claim the racing delete
                // contends on, so the round where the course is gone refuses
                // here — which is the refusal this test wants to see answered.
                let term = crate::db::term::a_test_term(&db).await;
                let class = crate::db::class_group::create(
                    &db,
                    &creator,
                    crate::domain::class_group::ClassName::try_new("9-A").unwrap(),
                    None,
                    None,
                    None,
                )
                .await?
                .get_id()
                .clone();
                let instance = crate::service::class_course::attach(&db, &class, &course, &creator)
                    .await?
                    .get_id()
                    .clone();
                create(
                    &db,
                    &creator,
                    &instance,
                    &term,
                    ExamTitle::try_new("quiz").unwrap(),
                    ExamDescription::try_new("").unwrap(),
                    ExamKind::try_new("yazili", &kinds).unwrap(),
                    ExamSchedule::try_new(None, None, None, None).unwrap(),
                    ExamAttemptLimit::try_new(1).unwrap(),
                    true,
                    false,
                    false,
                )
                .await
                .map(|_| ())
            })
        }
        crate::db::course::assert_no_child_outlives_a_course_delete(
            "exam",
            "SELECT count(*) FROM exam e JOIN class_course cc ON cc.id = e.class_course
              WHERE cc.course = $1",
            make,
        )
        .await;
    }

    /// A real `app_user` row: students and graders are foreign keys now. The
    /// label names the row's username; the id is minted, so repeated calls are
    /// new people, not the same row.
    async fn a_person(db: &Database, label: &str, role: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at, role) \
             VALUES ($1, $2, 0, $3)",
        )
        .bind(user.uuid())
        .bind(format!("{label}-{}", &user.key()[30..]))
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        user
    }

    fn edit(exam: &Exam) -> (ExamTitle, ExamDescription, ExamKind, ExamSchedule) {
        (
            exam.get_title().clone(),
            exam.get_description().clone(),
            exam.get_kind().clone(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
        )
    }

    /// The bite test for the compare-and-set that replaced the writer lease on
    /// `PATCH /exams/{id}`: a merge built on a snapshot the row has moved past
    /// must be refused (the handler then re-reads and re-merges), never
    /// written over somebody else's edit. Asserts the *stored* row.
    #[tokio::test]
    async fn a_merge_built_on_a_stale_snapshot_is_refused() {
        let (db, _leases) = crate::database::init_test_db().await;
        let stale = published(&db).await;
        let (_, description, kind, schedule) = edit(&stale);
        let landed = update_if_unchanged(
            &db,
            stale.clone(),
            ExamTitle::try_new("theirs").unwrap(),
            description,
            kind,
            schedule,
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(landed.is_some(), "the first write is on a fresh snapshot");

        let (_, description, kind, schedule) = edit(&stale);
        let refused = update_if_unchanged(
            &db,
            stale,
            ExamTitle::try_new("mine").unwrap(),
            description,
            kind,
            schedule,
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(refused.is_none(), "a stale merge must not be written");
        let stored = read(&db, landed.unwrap().get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_title().as_str(), "theirs");
    }

    /// The bite test for the re-draft guard now living *inside* the update's
    /// transaction: a mark that lands after the handler's pre-flight gate (the
    /// grade path is a lock reader, exactly like this one) must still stop the
    /// exam from being hidden.
    #[tokio::test]
    async fn re_drafting_is_refused_by_the_write_itself_once_a_mark_exists() {
        use crate::db::exam_result;
        use crate::domain::exam_result::Mark;
        let (db, _leases) = crate::database::init_test_db().await;
        let exam = published(&db).await;
        let student = a_person(&db, "student", "student").await;
        let grader = a_person(&db, "teacher", "teacher").await;
        exam_result::grade(
            &db,
            exam.get_id(),
            &student,
            1,
            Mark::try_new(80).unwrap(),
            &grader,
            exam.get_kind().as_str(),
        )
        .await
        .unwrap();

        let (title, description, kind, schedule) = edit(&exam);
        let refused = update_if_unchanged(
            &db,
            exam.clone(),
            title,
            description,
            kind,
            schedule,
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            true,
        )
        .await
        .expect_err("a graded exam cannot be re-drafted");
        assert!(refused.to_string().contains("back into a draft"));
        let stored = read(&db, exam.get_id()).await.unwrap().unwrap();
        assert!(!stored.is_draft(), "nothing may have been written");
    }

    #[tokio::test]
    async fn title_is_required() {
        assert!(ExamTitle::try_new("midterm").is_ok());
        assert!(ExamTitle::try_new("").is_err());
        assert!(ExamTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(ExamDescription::try_new("").is_ok());
    }

    #[tokio::test]
    async fn kind_must_be_in_the_allowed_list() {
        let allowed: Vec<ExamKindDef> = crate::domain::settings::Settings::defaults()
            .get_exam_kinds()
            .to_vec();
        // The K12 defaults, pinned by name: the acceptance set of a fresh
        // school is exactly these three, and the pre-remodel vocabulary
        // ("quiz", "midterm", …) is not in it.
        for kind in ["yazili", "sozlu", "uygulama"] {
            assert_eq!(ExamKind::try_new(kind, &allowed).unwrap().as_str(), kind);
        }
        for stale in ["quiz", "midterm", "exam"] {
            assert!(
                ExamKind::try_new(stale, &allowed).is_err(),
                "{stale} is not in the school's list"
            );
        }
        assert!(ExamKind::try_new("", &allowed).is_err());
        // A school-defined list swaps the acceptance set wholesale.
        let custom = vec![ExamKindDef::try_new("lab", 2).unwrap()];
        assert!(ExamKind::try_new("lab", &custom).is_ok());
        assert!(ExamKind::try_new("midterm", &custom).is_err());
        // Matching is exact, case included — the settings list is the wire
        // truth, not a case-folded suggestion.
        assert!(ExamKind::try_new("Lab", &custom).is_err());
    }

    #[tokio::test]
    async fn mode_must_be_known() {
        for mode in ["sync", "async", "open"] {
            assert_eq!(ExamMode::try_new(mode).unwrap().as_str(), mode);
        }
        assert!(ExamMode::try_new("live").is_err());
    }

    #[tokio::test]
    async fn attempt_limit_counts_or_never_runs_out() {
        let single = ExamAttemptLimit::single();
        assert_eq!(single.as_i64(), 1);
        assert!(!single.is_unlimited());
        assert!(single.allows_another(0));
        assert!(!single.allows_another(1));

        let three = ExamAttemptLimit::try_new(3).unwrap();
        assert!(three.allows_another(2));
        assert!(!three.allows_another(3));
        assert!(!three.allows_another(4));

        let unlimited = ExamAttemptLimit::try_new(0).unwrap();
        assert!(unlimited.is_unlimited());
        assert!(unlimited.allows_another(0));
        assert!(unlimited.allows_another(10_000));

        assert!(ExamAttemptLimit::try_new(-1).is_err());
        assert!(ExamAttemptLimit::try_new(101).is_err());
    }

    #[tokio::test]
    async fn schedule_invariants_hold() {
        let mode = |m| Some(ExamMode::try_new(m).unwrap());
        let at = |ms| Some(Timestamp::from_millis(ms));
        let dur = Some(ExamDuration::try_new(90 * 60 * 1000).unwrap());

        // Unscheduled: nothing set is fine, any time/duration without a mode is not.
        assert!(ExamSchedule::try_new(None, None, None, None).is_ok());
        assert!(ExamSchedule::try_new(None, at(1), None, None).is_err());
        assert!(ExamSchedule::try_new(None, None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(None, None, None, dur).is_err());

        // Sync: fixed window, no duration.
        assert!(ExamSchedule::try_new(mode("sync"), at(1), at(2), None).is_ok());
        assert!(ExamSchedule::try_new(mode("sync"), None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("sync"), at(1), None, None).is_err());
        // Over-long `dur` in a 1ms window, but the root cause is that sync
        // takes no duration at all — that message wins over the window one.
        assert!(matches!(
            ExamSchedule::try_new(mode("sync"), at(1), at(2), dur),
            Err(ValidationError::Invalid {
                reason: "only async and open exams take a duration",
                ..
            })
        ));

        // Async: window plus a per-student duration, which must fit inside it
        // (equal to the window is fine; the window `at(1), at(2)` above is
        // too narrow for `dur`, so give it one exactly `dur` wide instead).
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(1 + 90 * 60 * 1000), dur).is_ok());
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(2), dur).is_err());
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(2), None).is_err());

        // Open: no window, duration optional (limited or unlimited time).
        assert!(ExamSchedule::try_new(mode("open"), None, None, None).is_ok());
        assert!(ExamSchedule::try_new(mode("open"), None, None, dur).is_ok());
        assert!(ExamSchedule::try_new(mode("open"), at(1), None, None).is_err());
        assert!(ExamSchedule::try_new(mode("open"), None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("open"), at(1), at(2), dur).is_err());

        // The window must be a real interval, whatever the mode.
        assert!(ExamSchedule::try_new(mode("sync"), at(2), at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("async"), at(3), at(2), dur).is_err());
    }

    /// GUARD, not a retry measurement — read the last paragraph before
    /// trusting this test with the retry. See
    /// [`crate::db::course::delete`]'s race test for why the rate is
    /// counted rather than asserted per round.
    ///
    /// This site has no marker at all: the delete's statements share one
    /// transaction whose first failure aborts the rest, so a genuine conflict
    /// can be masked by a sibling error, and either way there is no
    /// [`crate::database::lost_the_race`] check and no retry.
    ///
    /// The racer is a mark: [`crate::db::exam_result::grade`] claims the exam's own
    /// `result_count` (and the kind's reference) before writing, so it contends
    /// with the `DELETE $ex` and with the kind_ref decrement inside the same
    /// transaction. The witness that the site is genuinely raced is read per
    /// round and asserted over the whole run — the subject twin's shape. A
    /// grade answered 404 says the delete landed before it (`write_mark`'s
    /// existence gate is the only thing that can answer that); a grade going
    /// through says that grade beat the delete. Which side wins a *round* is
    /// load luck: under a busy machine the delete lands wholly on one side of
    /// the burst in every round (measured 0/20 same-round splits, 3 runs in 5
    /// red, with the integration suite running concurrently), so a same-round
    /// split must not be the gate. Stored state cannot witness the straddle —
    /// the gate is what stops a mark outliving its exam, so the sweep now
    /// finds nothing to leave behind in *every* round, which is asserted
    /// below as a fact rather than read as a signal.
    ///
    /// It does not prove the retry either: measured at 0 conflicts in 100 raced
    /// rounds, and green with
    /// [`crate::database::transaction_with_retry`]'s loop cut to a single
    /// attempt — the grades serialize behind `cap`'s claim lock, so they mostly
    /// queue rather than collide. So this guards the status codes and the
    /// cascade (marks either survive whole or are swept whole, never a 500).
    /// The retry is measured on [`crate::db::course::delete`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_a_mark_never_answers_500() {
        use crate::db::exam_result;
        use crate::domain::exam_result::Mark;
        let (db, _leases) = crate::database::init_test_db().await;
        let teacher = a_person(&db, "teacher", "teacher").await;
        let (mut delete_500, mut grade_500) = (0, 0);
        let (mut delete_first, mut grade_first, mut swept) = (0, 0, 0);
        let (mut last_delete, mut last_grade) = (String::new(), String::new());
        for round in 0..20 {
            let exam = published(&db).await;
            let kind = exam.get_kind().as_str().to_string();

            // A burst of marks and a delete held back by a sweeping beat: this
            // site has no guard to lose to, so a single racer released with it
            // simply finishes on one side of it. Same recipe as
            // [`crate::db::course::delete`]'s race test.
            let drop_it = {
                let (exam, db) = (exam.clone(), db.clone());
                // A wide sweep, not the 0-3ms the other three use: each grade
                // takes two counter writes serialized behind `cap`'s claim
                // lock, so the burst runs tens of milliseconds and a short beat
                // always puts the delete in front of all six.
                let beat = std::time::Duration::from_millis(round * 2);
                tokio::spawn(async move {
                    tokio::time::sleep(beat).await;
                    delete(&db, exam).await
                })
            };
            // Six real students per round: the mark's app_user is a foreign
            // key now.
            let mut students = Vec::with_capacity(6);
            for _ in 0..6 {
                students.push(a_person(&db, "student", "student").await);
            }
            let marks: Vec<_> = (0..6)
                .map(|seat| {
                    let (id, db, kind) = (exam.get_id().clone(), db.clone(), kind.clone());
                    let (student, teacher) = (students[seat], teacher);
                    tokio::spawn(async move {
                        exam_result::grade(
                            &db,
                            &id,
                            &student,
                            1,
                            Mark::try_new(50).unwrap(),
                            &teacher,
                            &kind,
                        )
                        .await
                    })
                })
                .collect();
            let drop_it = drop_it.await.unwrap();
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
                last_delete = format!("{:?}", drop_it.as_ref().err());
            }
            let mut refused = 0;
            for mark in marks {
                let mark = mark.await.unwrap();
                match mark {
                    // The existence gate: this grade reached the store after
                    // the row was gone.
                    Err(AppError::NotFound) => refused += 1,
                    Err(AppError::Db(_)) => {
                        grade_500 += 1;
                        last_grade = format!("{mark:?}");
                    }
                    _ => {}
                }
            }
            // Which side won, read per round and asserted over the whole run:
            // a round with a refused grade saw the delete land first, a round
            // with a grade through saw a grade land first. One round can be
            // both — that is the same-round split, welcome but not required.
            if refused > 0 {
                delete_first += 1;
            }
            if refused < 6 {
                grade_first += 1;
            }
            if exam_result::list_for_exam(&db, exam.get_id())
                .await
                .unwrap()
                .is_empty()
            {
                swept += 1;
            }
        }
        eprintln!(
            "Exam::delete raced: {delete_500}/20 delete 500s, {grade_500} grade 500s, \
             {delete_first} rounds with a grade refused / {grade_first} with one through, \
             {swept} swept clean"
        );
        assert!(
            delete_first > 0 && grade_first > 0,
            "the sweep never crossed the window ({delete_first} rounds with a grade \
             refused / {grade_first} with one through)"
        );
        // Not a race signal, an invariant: the gate refuses a mark for an exam
        // that is gone, so no round can leave one behind for the next reader.
        assert_eq!(
            swept,
            20,
            "a mark outlived its exam in {} rounds",
            20 - swept
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            grade_500, 0,
            "a raced grade must retry, not 500: {grade_500}/20 rounds, last {last_grade}"
        );
    }

    /// One choice question on `exam`, with a real subject row behind it — a
    /// question claims a reference on its subject and is refused without one.
    async fn question_on(exam: &Exam, db: &Database) -> crate::domain::exam_question::ExamQuestion {
        use crate::domain::exam_question::{
            ChoiceInput, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
        };
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec![
                ChoiceInput {
                    id: Some("a".into()),
                    text: "5".into(),
                },
                ChoiceInput {
                    id: Some("b".into()),
                    text: "6".into(),
                },
            ]),
            Some("b".into()),
            &[],
        )
        .unwrap();
        let subject = crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        crate::db::exam_question::create(
            db,
            exam.get_id(),
            *subject.get_id(),
            QuestionText::try_new("3 + 3?").unwrap(),
            QuestionPoints::try_new(5).unwrap(),
            spec,
        )
        .await
        .unwrap()
    }

    /// The `Menu::delete` defect, one domain over: a student's answer must not
    /// outlive the exam it belongs to. Guarding the save by *reading* the exam
    /// would not do it — the read sees a row [`crate::db::exam::delete`] has removed but
    /// not committed, while its cascade swept a snapshot predating the save, so
    /// both commit and the answer is left pointing at an exam that is gone.
    /// [`crate::db::exam_answer::save`] writes the exam row instead (its
    /// `result_count`, back unchanged), so the two transactions touch one row
    /// and Postgres refuses one of them.
    ///
    /// One child per round, deliberately — in the menu twin, two children in one
    /// round hid the bug: the first writer made the delete lose and re-send, and
    /// the re-sent sweep removed the other's row. Both orders are forced by
    /// awaiting one side to completion before the other starts. Overlapped
    /// rounds may all lose the delete; that is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_answer_written_inside_a_delete_never_outlives_the_exam() {
        use crate::db::exam_answer;
        let (db, _leases) = crate::database::init_test_db().await;

        let student = a_person(&db, "student", "student").await;

        // Delete-first: the exam is gone before the save starts.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let pick = question.get_choices().unwrap()[1]
                .get_id()
                .as_str()
                .to_string();
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "delete-first: the exam is still there after delete: {dropped:?}"
            );
            let child = exam_answer::save(&db, &question, &student, 1, Some(pick), None).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: a save must be answered, not 500: {child:?}"
            );
            assert_eq!(
                exam_answer::list_for_exam(&db, &id).await.unwrap().len(),
                0,
                "an answer outlived its exam"
            );
        }

        // Write-first: the save lands, then the delete sweeps it.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let pick = question.get_choices().unwrap()[1]
                .get_id()
                .as_str()
                .to_string();
            let child = exam_answer::save(&db, &question, &student, 1, Some(pick), None).await;
            assert!(
                child.is_ok(),
                "write-first: the save must land before the delete: {child:?}"
            );
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "write-first: the exam is still there"
            );
            assert_eq!(
                exam_answer::list_for_exam(&db, &id).await.unwrap().len(),
                0,
                "an answer outlived its exam"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let pick = question.get_choices().unwrap()[1]
                .get_id()
                .as_str()
                .to_string();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, exam, gate) = (db.clone(), exam, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, exam).await
                })
            };
            let child = {
                let (db, question, student, pick, gate) =
                    (db.clone(), question, student, pick, gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    exam_answer::save(&db, &question, &student, 1, Some(pick), None).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced save must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    exam_answer::list_for_exam(&db, &id).await.unwrap().len(),
                    0,
                    "round {round}: an answer outlived its exam"
                );
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the exam is still there");
            }
        }
        eprintln!("Exam::delete raced by an answer save: {swept}/4 concurrent rounds deleted the exam");
    }

    /// The same defect on the teacher's side of the sheet, and a worse one: a
    /// question written inside the delete window kept the reference it claimed
    /// on its subject (the cascade's per-subject decrement counted only the
    /// rows it could see), and [`crate::db::subject::delete`] is gated on that
    /// count reading zero — a subject nobody could ever delete again, hanging
    /// off an exam nobody could ever see. The freeze gate is a *read* of
    /// `exam_attempt` and never survived this window;
    /// [`crate::db::exam_attempt::write_unfrozen_with`] now writes the exam row
    /// too. Both orders are forced by awaiting one side to completion before
    /// the other starts. Overlapped rounds may all lose the delete; that is
    /// not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_question_written_inside_a_delete_never_outlives_the_exam() {
        use crate::domain::exam_question::{
            ChoiceInput, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
        };
        let (db, _leases) = crate::database::init_test_db().await;

        fn spec() -> QuestionSpec {
            QuestionSpec::try_new(
                QuestionKind::try_new("choice").unwrap(),
                Some(vec![
                    ChoiceInput {
                        id: Some("a".into()),
                        text: "5".into(),
                    },
                    ChoiceInput {
                        id: Some("b".into()),
                        text: "6".into(),
                    },
                ]),
                Some("b".into()),
                &[],
            )
            .unwrap()
        }

        async fn ask(
            db: &Database,
            exam: &ExamId,
            on: crate::domain::subject::SubjectId,
        ) -> Result<crate::domain::exam_question::ExamQuestion, AppError> {
            crate::db::exam_question::create(
                db,
                exam,
                on,
                QuestionText::try_new("3 + 3?").unwrap(),
                QuestionPoints::try_new(5).unwrap(),
                spec(),
            )
            .await
        }

        async fn subject_stuck(db: &Database, subject: &crate::domain::subject::Subject) -> bool {
            crate::db::subject::delete(
                db,
                crate::db::subject::read(db, subject.get_id())
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .is_err()
        }

        async fn a_subject(db: &Database) -> crate::domain::subject::Subject {
            crate::db::subject::create(
                db,
                &crate::db::course::a_test_course(db).await,
                crate::domain::subject::SubjectName::try_new("topic").unwrap(),
                crate::domain::subject::SubjectDescription::try_new("").unwrap(),
            )
            .await
            .unwrap()
        }

        // Delete-first: the exam is gone before the question write starts.
        {
            let exam = published(&db).await;
            let id = exam.get_id().clone();
            let subject = a_subject(&db).await;
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "delete-first: the exam is still there after delete: {dropped:?}"
            );
            let child = ask(&db, &id, *subject.get_id()).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: a question write must be answered, not 500: {child:?}"
            );
            assert_eq!(
                crate::db::exam_question::list_for_exam(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .0
                    .len(),
                0,
                "a question outlived its exam"
            );
            assert!(
                !subject_stuck(&db, &subject).await,
                "an orphan question left its subject undeletable"
            );
        }

        // Write-first: the question lands, then the delete sweeps it and frees
        // the subject reference it claimed.
        {
            let exam = published(&db).await;
            let id = exam.get_id().clone();
            let subject = a_subject(&db).await;
            let child = ask(&db, &id, *subject.get_id()).await;
            assert!(
                child.is_ok(),
                "write-first: the question must land before the delete: {child:?}"
            );
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "write-first: the exam is still there"
            );
            assert_eq!(
                crate::db::exam_question::list_for_exam(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .0
                    .len(),
                0,
                "a question outlived its exam"
            );
            assert!(
                !subject_stuck(&db, &subject).await,
                "an orphan question left its subject undeletable"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let exam = published(&db).await;
            let id = exam.get_id().clone();
            let subject = a_subject(&db).await;

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, exam, gate) = (db.clone(), exam, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, exam).await
                })
            };
            let child = {
                let (db, exam_id, on, gate) = (db.clone(), id.clone(), *subject.get_id(), gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    ask(&db, &exam_id, on).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced question write must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    crate::db::exam_question::list_for_exam(&db, &id, None, 0)
                        .await
                        .unwrap()
                        .0
                        .len(),
                    0,
                    "round {round}: a question outlived its exam"
                );
                assert!(
                    !subject_stuck(&db, &subject).await,
                    "round {round}: an orphan question left its subject undeletable"
                );
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the exam is still there");
            }
        }
        eprintln!(
            "Exam::delete raced by a question write: {swept}/4 concurrent rounds deleted the exam"
        );
    }

    /// The student's half of the same picture problem: a drawing is a bare
    /// upsert — the exact pre-fix shape of [`crate::db::exam_answer::save`] — so it kept
    /// the hole the text answer just lost, blob and all. It now rides the same
    /// exam-row write the answer does. Both orders are forced by awaiting one
    /// side to completion before the other starts. Overlapped rounds may all
    /// lose the delete; that is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_drawing_written_inside_a_delete_never_outlives_the_exam() {
        use crate::domain::answer_image::AnswerImage;
        use crate::domain::note_file::FileContentType;
        let (db, _leases) = crate::database::init_test_db().await;
        let student = a_person(&db, "student", "student").await;

        async fn draw(
            db: &Database,
            exam: &ExamId,
            question: &crate::domain::exam_question::ExamQuestionId,
            student: &UserId,
        ) -> Result<(AnswerImage, Option<String>), AppError> {
            let image = AnswerImage::new(
                exam,
                question,
                student,
                1,
                FileContentType::try_new("image/png").unwrap(),
                3,
            );
            crate::db::answer_image::upsert(db, image).await
        }

        // Delete-first: the exam is gone before the drawing starts.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let on = question.get_id().clone();
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "delete-first: the exam is still there after delete: {dropped:?}"
            );
            let child = draw(&db, &id, &on, &student).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: a drawing write must be answered, not 500: {child:?}"
            );
            assert_eq!(
                crate::db::answer_image::list_for_exam(&db, &id)
                    .await
                    .unwrap()
                    .len(),
                0,
                "a drawing outlived its exam"
            );
        }

        // Write-first: the drawing lands, then the delete sweeps it.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let on = question.get_id().clone();
            let child = draw(&db, &id, &on, &student).await;
            assert!(
                child.is_ok(),
                "write-first: the drawing must land before the delete: {child:?}"
            );
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "write-first: the exam is still there"
            );
            assert_eq!(
                crate::db::answer_image::list_for_exam(&db, &id)
                    .await
                    .unwrap()
                    .len(),
                0,
                "a drawing outlived its exam"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, exam, gate) = (db.clone(), exam, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, exam).await
                })
            };
            let child = {
                let (db, exam_id, on, student, gate) = (
                    db.clone(),
                    id.clone(),
                    question.get_id().clone(),
                    student,
                    gate,
                );
                tokio::spawn(async move {
                    gate.wait().await;
                    draw(&db, &exam_id, &on, &student).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced drawing write must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    crate::db::answer_image::list_for_exam(&db, &id)
                        .await
                        .unwrap()
                        .len(),
                    0,
                    "round {round}: a drawing outlived its exam"
                );
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the exam is still there");
            }
        }
        eprintln!(
            "Exam::delete raced by a drawing write: {swept}/4 concurrent rounds deleted the exam"
        );
    }

    /// A picture is written through the same freeze gate as the question it
    /// hangs on, so it had the same hole — and one the row count does not even
    /// show: `delete_exam` collects the blob names to unlink *before* it calls
    /// [`crate::db::exam::delete`], so an image row landing after that snapshot
    /// strands its bytes on disk forever as well. It now rides the same
    /// exam-row write the question does. Both orders are forced by awaiting
    /// one side to completion before the other starts. Overlapped rounds may
    /// all lose the delete; that is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_picture_written_inside_a_delete_never_outlives_the_exam() {
        use crate::domain::note_file::FileContentType;
        use crate::domain::question_image::QuestionImage;
        let (db, _leases) = crate::database::init_test_db().await;

        async fn shoot(
            db: &Database,
            exam: &ExamId,
            question: &crate::domain::exam_question::ExamQuestionId,
        ) -> Result<(QuestionImage, Option<String>), AppError> {
            let image = QuestionImage::new(
                exam,
                question,
                None,
                FileContentType::try_new("image/png").unwrap(),
                3,
            );
            crate::db::question_image::upsert(db, image).await
        }

        // Delete-first: the exam is gone before the picture starts.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let on = question.get_id().clone();
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "delete-first: the exam is still there after delete: {dropped:?}"
            );
            let child = shoot(&db, &id, &on).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: a picture write must be answered, not 500: {child:?}"
            );
            assert_eq!(
                crate::db::question_image::list_for_exam(&db, &id)
                    .await
                    .unwrap()
                    .len(),
                0,
                "a picture outlived its exam"
            );
        }

        // Write-first: the picture lands, then the delete sweeps it.
        {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();
            let on = question.get_id().clone();
            let child = shoot(&db, &id, &on).await;
            assert!(
                child.is_ok(),
                "write-first: the picture must land before the delete: {child:?}"
            );
            let dropped = delete(&db, exam).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "write-first: the exam is still there"
            );
            assert_eq!(
                crate::db::question_image::list_for_exam(&db, &id)
                    .await
                    .unwrap()
                    .len(),
                0,
                "a picture outlived its exam"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let exam = published(&db).await;
            let question = question_on(&exam, &db).await;
            let id = exam.get_id().clone();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, exam, gate) = (db.clone(), exam, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, exam).await
                })
            };
            let child = {
                let (db, exam_id, on, gate) =
                    (db.clone(), id.clone(), question.get_id().clone(), gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    shoot(&db, &exam_id, &on).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced picture write must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    crate::db::question_image::list_for_exam(&db, &id)
                        .await
                        .unwrap()
                        .len(),
                    0,
                    "round {round}: a picture outlived its exam"
                );
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the exam is still there");
            }
        }
        eprintln!(
            "Exam::delete raced by a picture write: {swept}/4 concurrent rounds deleted the exam"
        );
    }
}
