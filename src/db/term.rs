//! The `term` table: the row mint, the paged calendar listing, the
//! field-scoped PATCH whose `WHERE` re-checks the merged range, the delete
//! guarded by the row's own reference counters, and the archive/unarchive
//! stamps. The type and the errors its guards hand out live in
//! [`crate::domain::term`].

use crate::constant::TERM_TABLE;
use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::term::{Term, TermId, TermName};
use crate::domain::timestamp::{Timestamp, range_error};
use crate::error::AppError;

pub async fn create(
    db: &Database,
    name: TermName,
    starts_at: Timestamp,
    ends_at: Timestamp,
) -> Result<Term, AppError> {
    let term = Term {
        id: TermId::generate(),
        name,
        starts_at,
        ends_at,
        archived_at: None,
    };
    let created = sqlx::query_as!(
        Term,
        r#"INSERT INTO term (id, name, starts_at, ends_at, archived_at)
           VALUES ($1, $2, $3, $4, NULL)
           RETURNING id AS "id: TermId", name AS "name: TermName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     archived_at AS "archived_at: Timestamp""#,
        term.id.uuid(),
        term.name.as_str(),
        term.starts_at.as_millis(),
        term.ends_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(created)
}

pub async fn read(db: &Database, id: &TermId) -> Result<Option<Term>, AppError> {
    let term = sqlx::query_as!(
        Term,
        r#"SELECT id AS "id: TermId", name AS "name: TermName",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                  archived_at AS "archived_at: Timestamp" FROM term WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(term)
}

/// Every term, newest first — the school calendar is small by nature.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Term>, i64), AppError> {
    PagedList::new("term", "ORDER BY starts_at DESC, id DESC")
        .run::<Term>(limit, offset, db)
        .await
}

/// Write only the fields the PATCH carried — `None` means the request
/// omitted it, so the column is left alone rather than re-stated from the
/// snapshot this struct was read into. All three columns are non-nullable,
/// so "absent" and "null" both correctly mean "keep".
pub async fn update(
    db: &Database,
    term: Term,
    name: Option<TermName>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<Term, AppError> {
    FieldUpdate::new(TERM_TABLE, term.id.uuid())
        .set("name", name.map(|name| name.as_str().to_owned()))
        .set("starts_at", starts_at.map(|at| at.as_millis()))
        .set("ends_at", ends_at.map(|at| at.as_millis()))
        .ordered("starts_at", "ends_at", range_error())
        .run::<Term>(db)
        .await
}

/// Delete the term, but only while no course *and no class* links it —
/// nothing here unlinks or cascades. `false` = refused, nothing was written.
///
/// The roster of linking courses is the term's own `course_count`
/// refcount, claimed by [`crate::db::course::create`] and
/// `update` *before* they write a link, so the check and the delete are one
/// conditional write on one record: a course write racing this either
/// claims first (and the delete is refused) or finds the row gone (and is
/// refused itself, with the same 400 the lookup gives). `Err(NotFound)`
/// keeps the answer a concurrent *delete* used to get.
///
/// The referencing foreign keys (`course.term`, `class_group.term`, both
/// `NO ACTION`) are the backstop behind the counters, not a second guard:
/// while the counts are honest the `WHERE` decides every race on its own,
/// because both claim paths lock this very row before they write a link.
pub async fn delete(db: &Database, term: Term) -> Result<bool, AppError> {
    let gone = sqlx::query!(
        r#"DELETE FROM term
           WHERE id = $1 AND course_count = 0 AND class_count = 0"#,
        term.id.uuid(),
    )
    .execute(db)
    .await?;
    if gone.rows_affected() > 0 {
        return Ok(true);
    }
    // Still linked or already gone: the one statement cannot tell those
    // apart, and only the refusal path pays for the read that can.
    match read(db, &term.id).await? {
        Some(_) => Ok(false),
        None => Err(AppError::NotFound),
    }
}

/// Freeze the term. Idempotent by construction: the `WHERE` only matches an
/// open row, so a repeat archive writes nothing and answers with the
/// *original* stamp — the year is not re-dated by a double click.
pub async fn archive(db: &Database, term: Term) -> Result<Term, AppError> {
    let written = sqlx::query_as!(
        Term,
        r#"UPDATE term SET archived_at = $2
           WHERE id = $1 AND archived_at IS NULL
           RETURNING id AS "id: TermId", name AS "name: TermName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     archived_at AS "archived_at: Timestamp""#,
        term.id.uuid(),
        Timestamp::now().as_millis(),
    )
    .fetch_optional(db)
    .await?;
    match written {
        Some(written) => Ok(written),
        // A repeat archive (or a term deleted under the request): the stored
        // row is the answer, and only the no-op path pays for the read.
        None => read(db, &term.id).await?.ok_or(AppError::NotFound),
    }
}

/// Re-open the term; idempotent the same way.
pub async fn unarchive(db: &Database, term: Term) -> Result<Term, AppError> {
    let written = sqlx::query_as!(
        Term,
        r#"UPDATE term SET archived_at = NULL
           WHERE id = $1 AND archived_at IS NOT NULL
           RETURNING id AS "id: TermId", name AS "name: TermName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     archived_at AS "archived_at: Timestamp""#,
        term.id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    match written {
        Some(written) => Ok(written),
        None => read(db, &term.id).await?.ok_or(AppError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bite test for the `WHERE` guard that replaced `TERM_LOCK` on the
    /// PATCH path: the handler's pre-flight check is not in play here, so only
    /// the guard can refuse a moved end that inverts the range — and it must
    /// refuse it with the same error, having written nothing.
    #[tokio::test]
    async fn a_moved_end_is_refused_against_the_stored_other_end() {
        let (db, _leases) = crate::database::init_test_db().await;
        let at = Timestamp::from_millis;
        let term = create(&db, TermName::try_new("2026").unwrap(), at(100), at(200))
            .await
            .unwrap();

        let refused = update(&db, term.clone(), None, None, Some(at(50)))
            .await
            .expect_err("an end before the stored start must be refused");
        assert!(refused.to_string().contains("at or after starts_at"));
        let stored = read(&db, term.get_id()).await.unwrap().unwrap();
        assert_eq!(
            stored.get_ends_at(),
            at(200),
            "nothing may have been written"
        );

        // A move that keeps the range ordered still lands, guard and all.
        let moved = update(&db, term, None, None, Some(at(300))).await.unwrap();
        assert_eq!(moved.get_ends_at(), at(300));
    }

    /// Terms are listed newest-first *and* paged by offset, so the sort has to
    /// be a total order: `starts_at` alone leaves rows that share an instant in
    /// an arbitrary order, and offset paging over an unstable order can hand
    /// the same row out twice while skipping another. Pins the `id DESC`
    /// tie-break on identical `starts_at` — stored read-back, then page by page.
    #[tokio::test]
    async fn identical_starts_at_still_pages_each_term_exactly_once() {
        let (db, _leases) = crate::database::init_test_db().await;
        let at = Timestamp::from_millis;
        let mut minted = Vec::new();
        for i in 0..12 {
            let term = create(
                &db,
                TermName::try_new(&format!("t{i}")).unwrap(),
                at(100),
                at(200),
            )
            .await
            .unwrap();
            minted.push(term.get_id().key().to_string());
        }
        // Newest first: the tie-break runs the same way as the primary column.
        minted.reverse();

        let (listed, total) = list_all(&db, None, 0).await.unwrap();
        assert_eq!(total, 12);
        let read_back: Vec<String> = listed
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        assert_eq!(read_back, minted);

        // The assertion that catches skip/duplicate: walk it in pages of 5.
        let mut paged = Vec::new();
        for offset in [0, 5, 10] {
            let (page, total) = list_all(&db, Some(5), offset).await.unwrap();
            assert_eq!(total, 12);
            paged.extend(page.iter().map(|row| row.get_id().key().to_string()));
        }
        assert_eq!(paged, minted, "every term exactly once, in list order");
    }

    /// GUARD, not a retry measurement — read the last paragraph before
    /// trusting this test with the retry. See
    /// [`crate::db::course::delete`]'s race test for why the rate is
    /// counted rather than asserted per round.
    ///
    /// One conditional `DELETE … RETURN BEFORE` and a bare `.check()?`: no
    /// transaction to abort, but also no [`crate::database::write_with_retry`],
    /// which every other guarded single-statement write in the crate goes
    /// through. A store answering "conflict, retry" therefore comes out as a
    /// 500 instead of the 404 or 409 the request owes.
    ///
    /// The racer is [`crate::db::course::create`] against this
    /// term: it claims `course_count` on the term row before it writes the
    /// link, which is the same record and the same column the guard reads. Both
    /// sides are swept across each other sub-millisecond, exactly as in
    /// [`crate::db::subject::delete`]'s race test — a whole
    /// millisecond of head start on either side separates them completely, and
    /// the counters below assert the sweep straddled the site rather than
    /// landing on one side of it (it used to alternate on `round % 2` and score
    /// an exact 10/10, i.e. no overlap at all).
    ///
    /// And like that test it does *not* prove the retry: this site is one
    /// statement, so the window in which a conflict could reach
    /// [`write_with_retry`] is a single round trip wide — measured at 0
    /// conflicts in 100 raced rounds, green with the retry loop cut to a single
    /// attempt. A status-code guard, then: a raced delete answers 409 or 404 and
    /// never 500, and a course that got linked survives it. The retry is
    /// measured on [`crate::db::course::delete`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_a_course_create_never_answers_500() {
        use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
        use crate::domain::user::UserId;
        let (db, _leases) = crate::database::init_test_db().await;
        // The course's teacher is a foreign key now: one real row, reused by
        // every create in every round.
        let teacher = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'teacher')",
        )
        .bind(teacher.uuid())
        .bind(format!("term-race-{}", &teacher.key()[30..]))
        .execute(&db)
        .await
        .unwrap();
        let (mut delete_500, mut create_500) = (0, 0);
        let (mut linked, mut wiped) = (0, 0);
        let (mut last_delete, mut last_create) = (String::new(), String::new());
        let at = Timestamp::from_millis;
        for round in 0..20 {
            let term = create(&db, TermName::try_new("2026").unwrap(), at(100), at(200))
                .await
                .unwrap();

            let separated = round % 4 == 0;
            let drop_it = {
                let (term, db) = (term.clone(), db.clone());
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
                    delete(&db, term).await
                })
            };
            let makes: Vec<_> = (0..6)
                .map(|_| {
                    let (id, db, teacher) = (*term.get_id(), db.clone(), teacher);
                    let head_start = if separated {
                        std::time::Duration::from_millis(2)
                    } else {
                        std::time::Duration::from_micros(round * 37 % 300)
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(head_start).await;
                        crate::db::course::create(
                            &db,
                            &teacher,
                            CourseTitle::try_new("algebra").unwrap(),
                            CourseDescription::try_new("").unwrap(),
                            CourseKind::course(),
                            Some(id),
                            None,
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
            // Stored state, both sides: a linked course means the claim beat the
            // guard, a gone term means the delete did.
            let mut landed = false;
            for make in makes {
                let make = make.await.unwrap();
                if matches!(make, Err(AppError::Db(_))) {
                    create_500 += 1;
                    last_create = format!("{make:?}");
                }
                if let Ok(course) = &make
                    && crate::db::course::read(&db, course.get_id())
                        .await
                        .unwrap()
                        .is_some()
                {
                    landed = true;
                }
            }
            linked += usize::from(landed);
            if read(&db, term.get_id()).await.unwrap().is_none() {
                wiped += 1;
            }
        }
        eprintln!(
            "Term::delete raced: {delete_500}/20 delete 500s, {create_500} create 500s, \
             {linked} rounds with a course linked / {wiped} wiped"
        );
        assert!(
            linked > 0 && wiped > 0,
            "the sweep never crossed the window ({linked} linked / {wiped} wiped)"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must be refused, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            create_500, 0,
            "a raced course create must retry, not 500: {create_500}/20 rounds, last {last_create}"
        );
    }
}
