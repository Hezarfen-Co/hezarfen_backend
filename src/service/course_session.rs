//! Course-session workflows: who may teach a lesson (the teacher-resolution
//! gate), and the doors the web layer takes for every session read and
//! write. The queries live in [`crate::db::course_session`]; the
//! timetable's wire shaping and the course-rights checks stay in the web
//! layer.
//!
//! The one workflow with a plan behind it is [`materialize`]: it expands an
//! instance's *resolved* weekly plan ([`crate::service::weekly_slot`]) into
//! dated lessons over a range, bounded by the school-wide holiday calendar
//! ([`crate::service::holiday::blocked_days`]). It is an explicit route's
//! work, not a scheduler: nothing generates lessons on its own.

use crate::constant::{MAX_MATERIALIZE_DAYS, MAX_MATERIALIZE_SESSIONS, MILLIS_PER_DAY};
use crate::database::{Database, tx_with_retry};
use crate::db::course_session::{self, NewSession};
use crate::domain::calendar;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::holiday::HolidayName;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Resolve who a session's teacher should be: the caller when `teacher_id` is
/// omitted (or names them), otherwise the referenced user — who must exist and
/// hold the `teacher` role or higher (a student cannot teach a lesson).
///
/// Scheduling hands the named person the session's teaching seat, so the
/// floor is checked on the *live* role here rather than at the web layer:
/// every future roll-call right derives from this column, and a stale
/// `teacher_id` in a request body must fail with the same 400 no matter
/// which route named it.
pub async fn resolve_session_teacher(
    teacher_id: Option<&str>,
    caller: &User,
    db: &Database,
) -> Result<User, AppError> {
    let target = match teacher_id {
        None => return Ok(caller.clone()),
        Some(key) if key == caller.get_id().key() => return Ok(caller.clone()),
        Some(key) => UserId::from_key(key),
    };
    let Some(user) = crate::db::user::read(db, &target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher does not exist",
        }));
    };
    if !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher must hold the teacher role or higher",
        }));
    }
    Ok(user)
}

/// Schedule a lesson on `class_course` — one dated occurrence of the instance,
/// which is what a roll call hangs off.
pub async fn create(
    db: &Database,
    class_course: &ClassCourseId,
    teacher: &UserId,
    topic: SessionTopic,
    starts_at: Timestamp,
    ends_at: Option<Timestamp>,
) -> Result<CourseSession, AppError> {
    course_session::create(db, class_course, teacher, topic, starts_at, ends_at).await
}

pub async fn read(db: &Database, id: &CourseSessionId) -> Result<Option<CourseSession>, AppError> {
    course_session::read(db, id).await
}

/// An instance's sessions, most recent lesson first — the timetable read.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseSession>, i64), AppError> {
    course_session::list_for_class_course(db, class_course, limit, offset).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all, so a concurrent PATCH of that field survives. The
/// range consistency re-check rides in the UPDATE's own `WHERE`, so the
/// caller holds nothing across its read and this write.
pub async fn update(
    db: &Database,
    session: CourseSession,
    teacher: Option<UserId>,
    topic: Option<SessionTopic>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<CourseSession, AppError> {
    course_session::update(db, session, teacher, topic, starts_at, ends_at).await
}

/// Delete the session and sweep its roll-call rows in one transaction.
pub async fn delete(db: &Database, session: CourseSession) -> Result<CourseSession, AppError> {
    course_session::delete(db, session).await
}

/// One non-teaching day the generator refused to schedule on: the calendar
/// day in the school's zone, and the holiday that reaches into it.
#[derive(Debug, Clone)]
pub struct BlockedDay {
    pub day: chrono::NaiveDate,
    pub holiday: HolidayName,
}

/// What one [`materialize`] call did — or, on a dry run, would have done.
///
/// `created` carries the rows the database actually inserted (empty on a dry
/// run), so a lesson a concurrent writer already put at the same instant is
/// reported as skipped, never counted. `candidates` is the size the range
/// asked for, `slots` the resolved plan's size, and `blocked` one entry per
/// holiday-blocked *day*.
#[derive(Debug, Clone)]
pub struct MaterializeOutcome {
    pub applied: bool,
    pub slots: usize,
    pub candidates: usize,
    pub created: Vec<CourseSession>,
    pub skipped_existing: usize,
    pub skipped_holiday: usize,
    pub blocked: Vec<BlockedDay>,
    pub range_days: i64,
}

/// Expand `instance`'s resolved weekly plan into dated lessons over
/// `[from, to]` (inclusive instants), one lesson per plan slot per matching
/// weekday, skipping the days a holiday reaches into and the instants the
/// section already holds a lesson at.
///
/// `apply == false` is a dry run: it reads, decides, and writes nothing.
/// `apply == true` writes the collected rows in this transaction and returns
/// exactly the rows that landed — the whole feature is additive, nothing is
/// ever deleted or updated, and a second run over the same range is a no-op
/// (`UNIQUE (class_course, starts_at)`). Weekends are not special-cased: a
/// school may run Saturday, so only the plan and the holidays decide.
///
/// `offset_minutes` is the school's zone offset
/// ([`crate::domain::calendar::zone_offset_minutes`]); the range's instants
/// name *school* days, and each lesson lands at its slot's minute of that
/// local day.
///
/// Refusals: an archived year, an empty plan (409
/// `instance_has_no_weekly_plan`), no assigned teacher (409
/// `instance_has_no_teacher`, the lesson's teacher is the instance's first
/// assigned one), a range longer than a year or one that would mint more than
/// [`MAX_MATERIALIZE_SESSIONS`] lessons (400), and a gone instance
/// ([`AppError::NotFound`]).
pub async fn materialize(
    db: &Database,
    instance: &ClassCourse,
    from: Timestamp,
    to: Timestamp,
    apply: bool,
    offset_minutes: i32,
) -> Result<MaterializeOutcome, AppError> {
    let instance_id = instance.get_id().clone();
    let days = calendar::days_between(
        calendar::zoned_day(from.as_millis(), offset_minutes),
        calendar::zoned_day(to.as_millis(), offset_minutes),
    );
    let range_days = days.len() as i64;
    if range_days > MAX_MATERIALIZE_DAYS {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "to",
            reason: "the range must not span more than a year",
        }));
    }

    // Every decision below is made while holding the instance's row lock, and
    // the generated rows are written on that same connection: a rival
    // materialize run waits here instead of racing the existence read, and a
    // refusal rolls back whatever it had collected.
    tx_with_retry(db, false, async move |tx| {
        // The archived-year wall every instance write passes: a past year is
        // read-only, and this run mints lesson rows.
        crate::service::class_course::require_open(db, &instance_id).await?;
        // The serialization point. `FOR NO KEY UPDATE` rather than
        // `FOR UPDATE`: the batch insert below takes this same row's
        // foreign-key `FOR KEY SHARE`, which `FOR UPDATE` would block on.
        crate::db::weekly_slot::lock_class_tx(tx, &instance_id).await?;

        let slots = crate::service::weekly_slot::resolved_for_instance(db, instance).await?;
        if slots.is_empty() {
            return Err(AppError::ConflictCoded {
                code: "instance_has_no_weekly_plan",
                message: "this section's weekly plan is empty — add a slot first".into(),
            });
        }
        let teachers =
            crate::db::class_course_teacher::list_for_instance(db, &instance_id).await?;
        let Some(teacher) = teachers.first().copied() else {
            return Err(AppError::ConflictCoded {
                code: "instance_has_no_teacher",
                message: "assign a teacher to this section before generating lessons".into(),
            });
        };

        // The topic chain: the slot's own first, the instance's *resolved*
        // title second (the one display chain every surface rides), the
        // literal `"Ders"` last. The resolved title is bounded by
        // `MAX_COURSE_TITLE_LEN`, so it always fits a `SessionTopic`.
        let resolved = crate::service::instance_resolve::resolved_content(db, &[instance]).await?;
        let fallback_topic = resolved
            .get(&instance_id.key())
            .map(|content| content.title.as_str())
            .filter(|title| !title.is_empty())
            .and_then(|title| SessionTopic::try_new(title).ok())
            .unwrap_or(SessionTopic::try_new("Ders")?);

        let holidays = crate::service::holiday::blocked_days(db, from, to).await?;
        let existing = course_session::existing_starts(db, &instance_id).await?;

        let mut rows: Vec<NewSession> = Vec::new();
        let mut blocked: Vec<BlockedDay> = Vec::new();
        let mut skipped_existing = 0usize;
        for day in &days {
            let day_start = calendar::day_start_millis(*day, offset_minutes);
            // Day granularity, not slot granularity: a day a holiday reaches
            // into is named once and contributes nothing, whether or not the
            // plan runs a lesson that weekday.
            let day_end = day_start + MILLIS_PER_DAY - 1;
            if let Some(holiday) = holidays.iter().find(|holiday| {
                holiday.overlaps(
                    Timestamp::from_millis(day_start),
                    Timestamp::from_millis(day_end),
                )
            }) {
                blocked.push(BlockedDay {
                    day: *day,
                    holiday: holiday.get_name().clone(),
                });
                continue;
            }
            let weekday = calendar::iso_weekday(*day);
            for slot in slots
                .iter()
                .filter(|slot| slot.get_weekday().get() == weekday)
            {
                let starts_at = day_start + slot.get_starts_at().get() * 60_000;
                if existing.contains(&starts_at) {
                    skipped_existing += 1;
                    continue;
                }
                rows.push(NewSession {
                    id: CourseSessionId::generate(),
                    class_course: instance_id.clone(),
                    teacher,
                    topic: slot
                        .get_topic()
                        .cloned()
                        .unwrap_or_else(|| fallback_topic.clone()),
                    starts_at: Timestamp::from_millis(starts_at),
                    ends_at: Timestamp::from_millis(
                        day_start + slot.get_ends_at().get() * 60_000,
                    ),
                });
            }
        }

        let candidates = rows.len();
        if candidates > MAX_MATERIALIZE_SESSIONS {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "to",
                reason: "this range would create more than 500 lessons — narrow it".into(),
            }));
        }
        let created = if apply {
            course_session::insert_many_tx(tx, &rows).await?
        } else {
            Vec::new()
        };
        Ok(MaterializeOutcome {
            applied: apply,
            slots: slots.len(),
            candidates,
            created,
            skipped_existing,
            skipped_holiday: blocked.len(),
            blocked,
            range_days,
        })
    })
    .await
}
