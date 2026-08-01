//! An academic term — semester, trimester, quarter, whatever this school
//! runs; the structure is just rows, so it needs no code change per school.
//! Courses may link to one term.
//!
//! Terms are calendar structure, not schedules: a school adopting the app
//! mid-year legitimately creates a term that already started, so the no-past
//! rule that guards exams/lessons/events deliberately does not apply here.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{COURSE_COUNT_FIELD, MAX_TERM_NAME_LEN, TERM_CLASS_COUNT_FIELD, TERM_TABLE};
use crate::database::{Database, write_with_retry};
use crate::domain::field_update::FieldUpdate;
use crate::domain::page::PagedList;
use crate::domain::timestamp::{Timestamp, range_error};
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct TermId(RecordId);

impl TermId {
    pub fn generate() -> Self {
        Self(RecordId::new(TERM_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(TERM_TABLE, key))
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
pub struct TermName(String);

impl TermName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_TERM_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One term on the school's academic calendar. Both ends are required — a
/// term is a date range by definition.
#[derive(Debug, Clone, SurrealValue)]
pub struct Term {
    id: TermId,
    name: TermName,
    starts_at: Timestamp,
    ends_at: Timestamp,
}

impl Term {
    pub fn get_id(&self) -> &TermId {
        &self.id
    }

    pub fn get_name(&self) -> &TermName {
        &self.name
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Timestamp {
        self.ends_at
    }

    pub async fn create(
        name: TermName,
        starts_at: Timestamp,
        ends_at: Timestamp,
        db: &Database,
    ) -> Result<Term, AppError> {
        let term = Term {
            id: TermId::generate(),
            name,
            starts_at,
            ends_at,
        };
        let created: Option<Term> = db.create(term.id.record()).content(term).await?;
        created.ok_or_else(|| AppError::Internal("failed to create term".into()))
    }

    pub async fn read(id: &TermId, db: &Database) -> Result<Option<Term>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every term, newest first — the school calendar is small by nature.
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Term>, i64), AppError> {
        PagedList::new("term", "ORDER BY starts_at DESC")
            .run(limit, offset, db)
            .await
    }

    /// Write only the fields the PATCH carried — `None` means the request
    /// omitted it, so the column is left alone rather than re-stated from the
    /// snapshot this struct was read into. All three columns are non-nullable,
    /// so "absent" and "null" both correctly mean "keep".
    pub async fn update(
        self,
        name: Option<TermName>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<Term, AppError> {
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("starts_at", starts_at)
            .set("ends_at", ends_at)
            .ordered("starts_at", "ends_at", range_error())
            .run::<Term>(db)
            .await
    }

    /// Delete the term, but only while no course *and no class* links it —
    /// nothing here unlinks or cascades. `false` = refused, nothing was written.
    ///
    /// The roster of linking courses is the term's own `course_count`
    /// refcount, claimed by [`crate::domain::course::Course::create`] and
    /// `update` *before* they write a link, so the check and the delete are one
    /// conditional write on one record: a course write racing this either
    /// claims first (and the delete is refused) or finds the row gone (and is
    /// refused itself, with the same 400 the lookup gives). `Err(NotFound)`
    /// keeps the answer a concurrent *delete* used to get.
    ///
    /// Classes ([`crate::domain::class_group::ClassGroup`]) link a term the same
    /// way and count on `class_count` — a column of their own, because
    /// `course_count` is seeded at boot from the course rows alone.
    pub async fn delete(self, db: &Database) -> Result<bool, AppError> {
        let sql = format!(
            "DELETE $term WHERE ({COURSE_COUNT_FIELD} ?? 0) = 0 \
             AND ({TERM_CLASS_COUNT_FIELD} ?? 0) = 0 RETURN BEFORE"
        );
        // Through the retry, because the guard reads the very column a course
        // create claims: a lost round writes nothing, and re-sending it is what
        // keeps the answer the 404 or 409 it owes instead of a 500.
        let gone: Vec<Term> =
            write_with_retry(db, &sql, &[("term".into(), self.id.record().into_value())]).await?;
        if !gone.is_empty() {
            return Ok(true);
        }
        // Still linked or already gone: the one statement cannot tell those
        // apart, and only the refusal path pays for the read that can.
        match Self::read(&self.id, db).await? {
            Some(_) => Ok(false),
            None => Err(AppError::NotFound),
        }
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
        let db = crate::database::init_mem().await.unwrap();
        let at = Timestamp::from_millis;
        let term = Term::create(TermName::try_new("2026").unwrap(), at(100), at(200), &db)
            .await
            .unwrap();

        let refused = term
            .clone()
            .update(None, None, Some(at(50)), &db)
            .await
            .expect_err("an end before the stored start must be refused");
        assert!(refused.to_string().contains("at or after starts_at"));
        let stored = Term::read(term.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(
            stored.get_ends_at(),
            at(200),
            "nothing may have been written"
        );

        // A move that keeps the range ordered still lands, guard and all.
        let moved = term.update(None, None, Some(at(300)), &db).await.unwrap();
        assert_eq!(moved.get_ends_at(), at(300));
    }

    #[tokio::test]
    async fn name_is_required_and_bounded() {
        assert!(TermName::try_new("2026 Fall").is_ok());
        assert!(TermName::try_new("").is_err());
        assert!(TermName::try_new("   ").is_err());
        assert!(TermName::try_new(&"x".repeat(101)).is_err());
    }

    /// GUARD, not a retry measurement — read the last paragraph before
    /// trusting this test with the retry. See
    /// [`crate::domain::course::Course::delete`]'s race test for why the rate is
    /// counted rather than asserted per round.
    ///
    /// One conditional `DELETE … RETURN BEFORE` and a bare `.check()?`: no
    /// transaction to abort, but also no [`crate::database::write_with_retry`],
    /// which every other guarded single-statement write in the crate goes
    /// through. A store answering "conflict, retry" therefore comes out as a
    /// 500 instead of the 404 or 409 the request owes.
    ///
    /// The racer is [`crate::domain::course::Course::create`] against this
    /// term: it claims `course_count` on the term row before it writes the
    /// link, which is the same record and the same column the guard reads. Both
    /// sides are swept across each other sub-millisecond, exactly as in
    /// [`crate::domain::subject::Subject::delete`]'s race test — a whole
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
    /// measured on [`crate::domain::course::Course::delete`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_a_course_create_never_answers_500() {
        use crate::domain::course::{Course, CourseDescription, CourseKind, CourseTitle};
        use crate::domain::user::UserId;
        let (db, _serialized) = crate::database::init_test_server("term_delete_race").await;
        let (mut delete_500, mut create_500) = (0, 0);
        let (mut linked, mut wiped) = (0, 0);
        let (mut last_delete, mut last_create) = (String::new(), String::new());
        let at = Timestamp::from_millis;
        for round in 0..20 {
            let term = Term::create(TermName::try_new("2026").unwrap(), at(100), at(200), &db)
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
                    term.delete(&db).await
                })
            };
            let makes: Vec<_> = (0..6)
                .map(|_| {
                    let (id, db) = (term.get_id().clone(), db.clone());
                    let head_start = if separated {
                        std::time::Duration::from_millis(2)
                    } else {
                        std::time::Duration::from_micros(round * 37 % 300)
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(head_start).await;
                        Course::create(
                            &UserId::from_key("teacher"),
                            CourseTitle::try_new("algebra").unwrap(),
                            CourseDescription::try_new("").unwrap(),
                            CourseKind::course(),
                            Some(id),
                            None,
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
                    && Course::read(course.get_id(), &db).await.unwrap().is_some()
                {
                    landed = true;
                }
            }
            linked += usize::from(landed);
            if Term::read(term.get_id(), &db).await.unwrap().is_none() {
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
