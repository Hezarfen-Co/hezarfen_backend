//! The `bank_question_image` slot table: the per-slot upsert, the page-wide
//! listing, and the choice sweep a non-destructive PATCH runs. Blob I/O
//! stays in the web layer; the pure entity and newtypes in
//! [`crate::domain::bank_question_image`].

use crate::database::{Database, foreign_key_violation, tx_with_retry};
use crate::domain::bank_question::BankQuestionId;
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam_question::ChoiceId;
use crate::domain::note_file::FileContentType;
use crate::error::AppError;

/// Create or replace the slot's image row — the table's `NULLS NOT DISTINCT`
/// unique key makes this the whole "one image per slot" story — handing back
/// what it stored plus the blob name it replaced, for the caller to take off
/// disk.
///
/// Both halves are the transaction's doing, and the row lock makes them one
/// switch: two uploads racing on one slot serialize on the `FOR UPDATE`, and
/// the loser's `ON CONFLICT` re-reads the winner's row, where two unlocked
/// pre-reads would both have seen the *old* blob name and left the loser's
/// fresh one orphaned on disk.
///
/// The template's existence rides the real foreign key: an insert whose
/// template was deleted mid-flight answers 23503 and maps to the same 404
/// the old points-move transaction produced with its `THROW`. Nothing is
/// written either way.
pub async fn upsert(
    db: &Database,
    image: BankQuestionImage,
) -> Result<(BankQuestionImage, Option<String>), AppError> {
    tx_with_retry(db, false, async move |tx| {
        let was = sqlx::query!(
            r#"SELECT file FROM bank_question_image
               WHERE bank_question = $1 AND slot IS NOT DISTINCT FROM $2
               FOR UPDATE"#,
            image.bank_question.uuid(),
            image.slot.as_ref().map(ChoiceId::as_str),
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.file);
        let stored = match sqlx::query_as!(
            BankQuestionImage,
            r#"INSERT INTO bank_question_image
                   (bank_question, slot, file, content_type, size)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (bank_question, slot) DO UPDATE
               SET file = $3, content_type = $4, size = $5
               RETURNING bank_question AS "bank_question: BankQuestionId",
                   slot AS "slot: ChoiceId", file,
                   content_type AS "content_type: FileContentType", size"#,
            image.bank_question.uuid(),
            image.slot.as_ref().map(ChoiceId::as_str),
            image.file.clone(),
            image.content_type.as_str(),
            image.size,
        )
        .fetch_one(&mut *tx)
        .await
        {
            Ok(row) => row,
            // The template row is gone; no orphan image may outlive it.
            Err(err) if foreign_key_violation(&err) => return Err(AppError::NotFound),
            Err(err) => return Err(err.into()),
        };
        Ok((stored, was))
    })
    .await
}

pub async fn read_slot(
    db: &Database,
    question: &BankQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<BankQuestionImage>, AppError> {
    let row = sqlx::query_as!(
        BankQuestionImage,
        r#"SELECT bank_question AS "bank_question: BankQuestionId",
               slot AS "slot: ChoiceId", file,
               content_type AS "content_type: FileContentType", size
           FROM bank_question_image
           WHERE bank_question = $1 AND slot IS NOT DISTINCT FROM $2"#,
        question.uuid(),
        slot.map(ChoiceId::as_str),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn list_for_question(
    db: &Database,
    question: &BankQuestionId,
) -> Result<Vec<BankQuestionImage>, AppError> {
    let rows = sqlx::query_as!(
        BankQuestionImage,
        r#"SELECT bank_question AS "bank_question: BankQuestionId",
               slot AS "slot: ChoiceId", file,
               content_type AS "content_type: FileContentType", size
           FROM bank_question_image WHERE bank_question = $1"#,
        question.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The image rows of several templates in one query — for bucketing onto a
/// listing's *page* (never the whole table: the bank spans the school).
pub async fn list_for_questions(
    db: &Database,
    questions: &[&BankQuestionId],
) -> Result<Vec<BankQuestionImage>, AppError> {
    if questions.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = questions.iter().map(|q| q.uuid()).collect();
    let rows = sqlx::query_as!(
        BankQuestionImage,
        r#"SELECT bank_question AS "bank_question: BankQuestionId",
               slot AS "slot: ChoiceId", file,
               content_type AS "content_type: FileContentType", size
           FROM bank_question_image WHERE bank_question = ANY($1)"#,
        &ids
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Drop the option pictures whose choice is gone — every choice image of
/// the question whose `slot` is *not* in `keep` (the question's own
/// illustration always stays), returning the removed rows so the caller can
/// take their blobs off disk.
///
/// This is what makes an edit non-destructive: a PATCH that reorders,
/// renames, or drops options passes the surviving choice ids as `keep`, so
/// only the pictures of genuinely removed options go. `keep = &[]` (a text
/// question, or an all-new choice list) still clears the lot.
pub async fn delete_choices_not_in(
    db: &Database,
    question: &BankQuestionId,
    keep: &[ChoiceId],
) -> Result<Vec<BankQuestionImage>, AppError> {
    let keep: Vec<String> = keep.iter().map(|id| id.as_str().to_string()).collect();
    let rows = sqlx::query_as!(
        BankQuestionImage,
        r#"DELETE FROM bank_question_image
           WHERE bank_question = $1
             AND slot IS NOT NULL
             AND NOT (slot = ANY($2))
           RETURNING bank_question AS "bank_question: BankQuestionId",
               slot AS "slot: ChoiceId", file,
               content_type AS "content_type: FileContentType", size"#,
        question.uuid(),
        &keep
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

pub async fn delete(
    db: &Database,
    image: BankQuestionImage,
) -> Result<BankQuestionImage, AppError> {
    let deleted = sqlx::query_as!(
        BankQuestionImage,
        r#"DELETE FROM bank_question_image
           WHERE bank_question = $1 AND slot IS NOT DISTINCT FROM $2
           RETURNING bank_question AS "bank_question: BankQuestionId",
               slot AS "slot: ChoiceId", file,
               content_type AS "content_type: FileContentType", size"#,
        image.bank_question.uuid(),
        image.slot.as_ref().map(ChoiceId::as_str)
    )
    .fetch_optional(db)
    .await?;
    deleted.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn png() -> crate::domain::note_file::FileContentType {
        crate::domain::note_file::FileContentType::try_new("image/png").unwrap()
    }

    /// A real template row: an image write moves its `points` inside its own
    /// transaction, so a minted id nothing ever wrote is refused.
    async fn a_template(db: &Database) -> BankQuestionId {
        let id = BankQuestionId::generate();
        db.query(
            "CREATE $b SET owner = user:u, text = 'soru', kind = 'text', points = 5,
             visibility = 'private', created_at = 1",
        )
        .bind(("b", id.record()))
        .await
        .unwrap()
        .check()
        .unwrap();
        id
    }

    fn choice_ids() -> Vec<ChoiceId> {
        use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};
        QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(
                ["a", "b", "c"]
                    .iter()
                    .map(|l| ChoiceInput {
                        id: Some((*l).into()),
                        text: (*l).into(),
                    })
                    .collect(),
            ),
            Some("a".into()),
            &[],
        )
        .unwrap()
        .into_parts()
        .1
        .unwrap()
        .iter()
        .map(|c| c.get_id().clone())
        .collect()
    }

    #[tokio::test]
    async fn upsert_replaces_per_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let question = a_template(&db).await;

        let (first, retired) = upsert(&db, BankQuestionImage::new(&question, None, png(), 3))
            .await
            .unwrap();
        assert_eq!(retired, None, "a first upload retires no blob");
        let (second, retired) = upsert(&db, BankQuestionImage::new(&question, None, png(), 5))
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer, and the
        // write itself names the blob the caller must unlink.
        assert_ne!(first.get_file(), second.get_file());
        assert_eq!(retired.as_deref(), Some(first.get_file()));
        let rows = list_for_question(&db, &question).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);
    }

    /// The bank half of the non-destructive edit: keeping an option keeps its
    /// picture, and only a removed option's picture goes.
    #[tokio::test]
    async fn only_the_dropped_options_lose_their_pictures() {
        let db = crate::database::init_mem().await.unwrap();
        let question = a_template(&db).await;
        let ids = choice_ids();
        upsert(&db, BankQuestionImage::new(&question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids {
            upsert(&db, BankQuestionImage::new(&question, Some(id), png(), 1))
                .await
                .unwrap();
        }

        let keep = vec![ids[0].clone(), ids[1].clone()];
        let dropped = delete_choices_not_in(&db, &question, &keep).await.unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].get_slot(), Some(&ids[2]));

        let left = list_for_question(&db, &question).await.unwrap();
        assert_eq!(left.len(), 3);
    }

    #[tokio::test]
    async fn an_empty_keep_set_clears_every_option_picture() {
        let db = crate::database::init_mem().await.unwrap();
        let question = a_template(&db).await;
        let ids = choice_ids();
        upsert(&db, BankQuestionImage::new(&question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids[..2] {
            upsert(&db, BankQuestionImage::new(&question, Some(id), png(), 1))
                .await
                .unwrap();
        }

        let dropped = delete_choices_not_in(&db, &question, &[]).await.unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|image| image.get_slot().is_some()));

        let left = list_for_question(&db, &question).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].get_slot(), None);
    }

    /// The [`crate::db::pool_question`] defect, one domain
    /// over: an image written inside its template's delete window must not
    /// outlive it. Guarding the write by *reading* the template does not do it
    /// — the read sees a row [`crate::db::bank_question::delete`] has removed
    /// but not committed, while its `DELETE bank_question_image WHERE
    /// bank_question = $b` ran on a snapshot predating this row, so both
    /// commit and the image is left pointing at a template that is gone.
    /// Nothing can reach it after that: every image route resolves the
    /// template first, no boot sweep visits this table, and its blob stays on
    /// disk for good. [`upsert`] moves the template's own `points` instead, so
    /// the two transactions touch one key and the store refuses a side.
    ///
    /// The blob half is asserted through the list the delete hands back: that
    /// list — never a snapshot read before it — is what the web layer unlinks
    /// from, so an image that committed inside the window and is *not* in it is
    /// a blob stranded on disk.
    ///
    /// The window is opened by the schema, not by a lucky interleaving: a
    /// `DEFINE EVENT` on `bank_question` fires inside the delete's own
    /// transaction the instant the row goes.
    ///
    /// Real server, and `#[ignore]`d for it: the subject *is* the store's
    /// conflict detection, which `init_mem`'s embedded engine does not have —
    /// it commits both and answers `Ok` to each, so this passes there on broken
    /// code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn an_image_written_inside_a_delete_window_never_outlives_its_template() {
        let (db, _serialized) = crate::database::init_test_server("bank_image_race").await;
        db.query(
            "DEFINE EVENT hold_the_window ON TABLE bank_question WHEN $event = 'DELETE' \
             THEN { SLEEP 1s; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let (mut swept, mut orphans, mut stranded) = (0, 0, 0);
        for round in 0..4 {
            let question = a_template(&db).await;
            let template = crate::db::bank_question::read(&db, &question)
                .await
                .unwrap()
                .unwrap();

            let drop_it = {
                let db = db.clone();
                tokio::spawn(async move { crate::db::bank_question::delete(&db, template).await })
            };
            // The upload starts inside the held window — the template row is
            // gone but uncommitted, which is exactly what the handler's
            // ownership read believes.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let child = {
                let (db, question) = (db.clone(), question.clone());
                tokio::spawn(async move {
                    upsert(&db, BankQuestionImage::new(&question, None, png(), 3)).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            // A 404 for the upload is a correct answer; the only defect is
            // stored state. Neither side may 500 — they contend by design and a
            // lost round is re-sent, not reported.
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced upload must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must retry, not 500: {drop_it:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if crate::db::bank_question::read(&db, &question)
                .await
                .unwrap()
                .is_none()
            {
                swept += 1;
                orphans += list_for_question(&db, &question).await.unwrap().len();
                if let (Ok((stored, _)), Ok((_, images))) = (&child, &drop_it)
                    && !images
                        .iter()
                        .any(|image| image.get_file() == stored.get_file())
                {
                    stranded += 1;
                }
            }
        }
        eprintln!(
            "bank_question::delete raced by an upload: {swept}/4 rounds deleted the template"
        );
        assert!(
            swept > 0,
            "no round ever deleted the template, so the window was never reached"
        );
        assert_eq!(orphans, 0, "an image row outlived its template");
        assert_eq!(
            stranded, 0,
            "an image committed inside the window was swept without being handed back, so its blob stays on disk"
        );
    }

    /// Two uploads to one slot must leave exactly one live blob and hand every
    /// other one back to be unlinked. Reading the slot's old blob name *before*
    /// the write gave both racers the same answer: both retired the same old
    /// blob, and the loser's fresh one stayed on disk with nothing naming it.
    /// [`upsert`] reads it inside its own transaction instead, so the
    /// loser contends on the row it writes and re-reads the winner's name.
    ///
    /// The overlap is forced rather than hoped for: a `DEFINE EVENT` on this
    /// table holds the first write open inside its own transaction, so the
    /// second reads the slot before the first commits.
    ///
    /// Real server, and `#[ignore]`d for it, for the reason above: the embedded
    /// engine has no write-write conflict detection to make a loser re-read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn two_uploads_to_one_slot_leave_no_blob_unretired() {
        let (db, _serialized) = crate::database::init_test_server("bank_image_replace").await;
        db.query(
            "DEFINE EVENT hold_the_write ON TABLE bank_question_image WHEN true \
             THEN { SLEEP 300ms; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let mut unretired = 0;
        for round in 0..3 {
            let question = a_template(&db).await;
            // A first picture, so both racers have an old blob to retire.
            let (original, _) = upsert(&db, BankQuestionImage::new(&question, None, png(), 1))
                .await
                .unwrap();

            let mut writes = Vec::new();
            for size in [2, 3] {
                let (db, question) = (db.clone(), question.clone());
                writes.push(tokio::spawn(async move {
                    upsert(&db, BankQuestionImage::new(&question, None, png(), size)).await
                }));
            }
            let mut written = vec![original.get_file().to_string()];
            let mut retired = Vec::new();
            for write in writes {
                let (stored, replaced) = write.await.unwrap().unwrap_or_else(|err| {
                    panic!("round {round}: a raced upload must not 500: {err:?}")
                });
                written.push(stored.get_file().to_string());
                retired.extend(replaced);
            }

            // Stored state is the whole verdict: exactly one blob is live, and
            // every other one this round wrote must have been handed back —
            // once. A name written and never returned is a file on disk with
            // nothing pointing at it.
            let live = read_slot(&db, &question, None).await.unwrap().unwrap();
            assert!(
                !retired.iter().any(|name| name == live.get_file()),
                "round {round}: the live row's own blob was retired"
            );
            unretired += written
                .iter()
                .filter(|name| *name != live.get_file() && !retired.contains(name))
                .count();
        }
        assert_eq!(
            unretired, 0,
            "a blob was written and never handed back to be unlinked — it stays on disk forever"
        );
    }
}
