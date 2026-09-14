//! The pump behind the class layer: one link row changed, and every
//! `enrollment` row that link implies reconciled with it, in one transaction.
//!
//! A class section (şube) has two link tables — `class_member` (a student in
//! it) and `class_course` (a course attached to it) — and the *product* of the
//! two is the roster it owes: every member is enrolled in every attached
//! course, in real `enrollment` rows tagged [`source`](crate::domain::enrollment)
//! with the class that wrote them. Adding a member and attaching a course are
//! therefore the same operation seen along its two axes, and so are removing
//! one and detaching the other.
//!
//! What is *not* one primitive is attach and detach. They share no statement:
//! one gates on "this pair already holds a row" and claims a counter upwards,
//! the other gates on nothing, releases, and has to decide per enrollment row
//! whether a *rival* class still claims it.
//!
//! Every invariant is one Postgres statement or one row lock, in the shapes
//! [`crate::db::cap`] documents: the class counter is claimed by a conditional
//! `UPDATE` fused with the link's `INSERT` (one CTE — a refused insert takes
//! its own seat bump back), a duplicate is the link table's natural composite
//! primary key answering as `23505`, a gone parent is a real `FOREIGN KEY`
//! answering as `23503`, and the row locks (`FOR KEY SHARE` / `FOR NO KEY
//! UPDATE`) put each transaction on the very record a racing writer must
//! touch. A refusal is an early `Err` (or an `Ok` verdict) from the
//! [`crate::database::tx_with_retry`] closure — a decision, never a retry.

use sqlx::postgres::PgConnection;

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::{Database, foreign_key_violation, tx_with_retry, unique_violation};
use crate::db::cap;
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::ClassMember;
use crate::domain::course::CourseId;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// What [`attach_course`]'s attach settled. The caller answers each verdict —
/// a hand attach turns them into this route's errors; a blueprint pump reads
/// them and carries on ([`crate::domain::class_blueprint`]).
#[derive(Debug)]
pub(crate) enum Attached<T> {
    /// The link row, the counter and every enrollment it implied committed
    /// together.
    Made(T),
    /// The link already exists — nothing was written, and no seat was spent
    /// finding that out.
    Duplicate,
    /// The class row is gone (a concurrent delete won). Nothing was written.
    Gone,
    /// The *pivot* — on the course axis, the course being attached — is gone.
    /// Nothing was written. Told apart from [`Attached::Gone`] because the two
    /// send a caller to look at different records, and a report that guesses
    /// between them names the wrong one half the time.
    ///
    /// On the **member** axis the pivot is the student, and this means their
    /// row is gone *or* their role no longer is `student`. Its one caller
    /// answers that as the `400` the route's own read already answers, never
    /// as [`Attached::refusal_code`]'s `course_deleted` — which is the course
    /// axis's word for it and would name a record this refusal is not about.
    PivotGone,
    /// The class is at its own ceiling on this axis. Nothing was written, and
    /// it is told apart from [`Attached::Gone`] because a full class is a
    /// standing row someone can make room in, not a 404.
    ClassFull,
    /// The class stands *above* the ceiling on the other axis, so the write
    /// loop this attach would run is longer than any transaction is allowed to
    /// be. Only a class predating the ceilings can be here, and only its own
    /// axis can make room — which is why it is not [`Attached::ClassFull`].
    ClassOverloaded,
    /// One of the courses had no free seat, named by its key. Nothing was
    /// written — not one of the earlier seats in the same run, which is the
    /// whole point of doing this in a transaction.
    Full(String),
    /// One of the courses the class carries no longer exists, named by its id:
    /// a stale `class_course` link. Nothing was written, and no capacity anyone
    /// can raise will change that answer.
    CourseGone(String),
    /// The blueprint this attach was sourced from is gone — a delete landed
    /// between the pump reading the template and this transaction running.
    /// Nothing was written, which is the point: a row tagged with a blueprint
    /// that no longer exists is one nothing can ever sweep. Only a sourced
    /// attach can be answered this.
    SourceGone,
}

impl<T> Attached<T> {
    /// This refusal as a **machine code**, or `None` for the attach that
    /// landed. The client owns the wording and the language; this only says
    /// *which* refusal it was — the shape every other enum here has (roles,
    /// course kinds, the badge catalog).
    ///
    /// The single home for that vocabulary, because both readers answer it: a
    /// blueprint pump reports it as a skip
    /// ([`crate::domain::class_blueprint`]) and a hand attach as the `code` on
    /// its `409` ([`crate::error::AppError::ConflictCoded`]). Spelled once, so
    /// one cause can never grow two codes.
    ///
    /// Each code names the record that actually failed. The two "gone" answers
    /// are a *class* delete ([`Attached::Gone`] → `class_deleted`) and a
    /// *course* delete ([`Attached::PivotGone`] → `course_deleted`) — and
    /// telling a manager the class vanished when the course did sends them to
    /// look at a section that is standing right there. `linked_course_missing`
    /// is a third: *another* course already attached to this class no longer
    /// exists, and it must be detached before this attach can be retried.
    /// `blueprint_deleted` is a pump losing the template itself mid-run — the
    /// only refusal that says nothing about the (class, course) pair it names.
    ///
    /// The `axis` is a parameter because two of these answers name a *different
    /// ceiling* on each of them: [`Attached::ClassFull`] is the ceiling of the
    /// axis being attached and [`Attached::ClassOverloaded`] the other axis's,
    /// so a member add's "full" is a full roster where a course attach's is a
    /// full course list ([`Axis::at_ceiling_code`]). Read blind, the same code
    /// worded a full roster as a full course list.
    pub(crate) fn refusal_code(&self, axis: &Axis) -> Option<&'static str> {
        match self {
            Attached::Made(_) => None,
            Attached::Duplicate => Some("duplicate"),
            Attached::Gone => Some("class_deleted"),
            Attached::PivotGone => Some("course_deleted"),
            Attached::ClassFull => Some(axis.at_ceiling_code()),
            Attached::ClassOverloaded => Some(axis.other().over_ceiling_code()),
            Attached::Full(_) => Some("course_full"),
            Attached::CourseGone(_) => Some("linked_course_missing"),
            Attached::SourceGone => Some("blueprint_deleted"),
        }
    }
}

/// Which way the pump runs: the loop below needs a `(course, user)` pair per
/// enrollment, and each caller supplies one side as a constant and reads the
/// other off the class's *other* link table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    /// A student joining: the courses come from `class_course`.
    Member,
    /// A course arriving: the students come from `class_member`.
    Course,
}

impl Axis {
    /// How many link rows this axis's counter may reach.
    ///
    /// This is what makes the pair loop in [`add_member`]/[`attach_course`]
    /// finite, and it does it *crosswise*: the member axis's loop iterates the
    /// class's `class_course` rows, which the course axis's counter caps, and
    /// the course axis's loop iterates its `class_member` rows, which the
    /// member axis's counter caps. So bounding the two counters bounds both
    /// write loops — one transaction can never carry more than
    /// `MAX_CLASS_MEMBERS`/`MAX_CLASS_COURSES` enrollment writes.
    ///
    /// Crosswise is also why the claim alone is not enough. It bounds the axis
    /// being *added*, and the loop it runs is the length of the *other* one: a
    /// class that already holds more members than `MAX_CLASS_MEMBERS` — the
    /// layer shipped before either ceiling existed, so a real volume can carry
    /// one — could still have a course attached, and that attach writes one
    /// enrollment per member. So the attach checks the other axis too
    /// ([`Axis::other`]), and the bound holds for stale classes as well.
    fn cap(self) -> i64 {
        match self {
            Axis::Member => MAX_CLASS_MEMBERS,
            Axis::Course => MAX_CLASS_COURSES,
        }
    }

    /// The refusal code for "the class is *at* this axis's ceiling"
    /// ([`Attached::ClassFull`]), taken off the axis beside the `cap` it
    /// reports, because that ceiling and its name are one fact.
    fn at_ceiling_code(self) -> &'static str {
        match self {
            Axis::Member => "class_at_roster_ceiling",
            Axis::Course => "class_at_course_ceiling",
        }
    }

    /// The refusal code for "the class stands *above* this axis's ceiling", so
    /// the other axis's attach would run a write loop longer than a transaction
    /// may be ([`Attached::ClassOverloaded`]). Answered off the axis that is
    /// over, never the one being attached — the two are always different axes,
    /// which is exactly what the blind version got wrong.
    fn over_ceiling_code(self) -> &'static str {
        match self {
            Axis::Member => "class_roster_too_large",
            Axis::Course => "class_course_list_too_large",
        }
    }

    /// The axis whose link rows this one's write loop iterates — its counter is
    /// the length of that loop, and its ceiling is therefore the second half of
    /// the bound.
    fn other(self) -> Axis {
        match self {
            Axis::Member => Axis::Course,
            Axis::Course => Axis::Member,
        }
    }
}

/// The pivot of an attach: the row the link hangs off on its own axis, which
/// must still be there — and, on the member axis, still name a `student` —
/// when the transaction runs.
enum Pivot<'a> {
    User(&'a UserId),
    Course(&'a CourseId),
}

/// The verdicts the shared prechecks can settle before any counter moves.
enum Early {
    Duplicate,
    SourceGone,
    PivotGone,
    Overloaded,
}

fn refusal_of<T>(early: Early) -> Attached<T> {
    match early {
        Early::Duplicate => Attached::Duplicate,
        Early::SourceGone => Attached::SourceGone,
        Early::PivotGone => Attached::PivotGone,
        Early::Overloaded => Attached::ClassOverloaded,
    }
}

/// The gates that run before any counter moves, in the order their answers
/// outrank each other:
///
/// 1. **Duplicate** — "you are already in" outranks "there is no room": a
///    caller whose row a rival placed a moment ago must not be told a course
///    is full about seats they already hold. (The link table's own primary key
///    re-answers this below as `23505` for a rival landing inside the window —
///    same verdict, no second seat.)
/// 2. **Source** — the blueprint that asked for this attach is still there. A
///    `FOR KEY SHARE` row lock on the blueprint row: it serializes the attach
///    against the blueprint's own delete — the closure the old process-wide
///    lease provided, now on the row itself. A concurrent blueprint `DELETE`
///    waits behind it and its sweep finds the committed row, while an attach
///    starting after the delete finds no row and writes nothing. `KEY SHARE`
///    is the weakest strength that blocks the delete, so two sourced attaches
///    still run concurrently, exactly as they did under the old shared read
///    lease.
///
///    The same locked read resolves the surrogate `id` the link row will
///    store: the stored tag is that uuid, not the grade label the API
///    speaks, and it rides out of this gate to the insert below.
/// 3. **Pivot** — the member axis takes the [`cap` role-claim
///    recipe](crate::db::cap)'s `FOR NO KEY UPDATE` handshake on the user row:
///    a role change away from `student` sweeps this membership, and demotion
///    and join now serialize on that row lock in both directions. The course
///    axis takes a `FOR KEY SHARE` on the course row, which a concurrent
///    course delete cannot take — the old bump-and-restore existence proof,
///    now a plain lock.
/// 4. **Overloaded** — the *other* axis standing above its own ceiling, which
///    no claim on this axis can see, and which is what bounds this
///    transaction's write loop.
async fn early_verdicts(
    tx: &mut PgConnection,
    class: &ClassGroupId,
    axis: Axis,
    pivot: Pivot<'_>,
    source: Option<&ClassBlueprintId>,
) -> Result<(Option<Early>, Option<uuid::Uuid>), AppError> {
    let held = match pivot {
        Pivot::User(user) => sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM class_member WHERE class = $1 AND app_user = $2"#,
            class as _,
            user as _
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some(),
        Pivot::Course(course) => sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM class_course WHERE class = $1 AND course = $2"#,
            class as _,
            course as _
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some(),
    };
    if held {
        return Ok((Some(Early::Duplicate), None));
    }
    // The lock and the resolution are one read: the stored `source` is the
    // blueprint's surrogate uuid, and the row this read returns it from is
    // the very row the KEY SHARE lock holds against the blueprint's delete.
    let mut source_id = None;
    if let Some(source) = source {
        let locked = sqlx::query_scalar!(
            r#"SELECT id FROM class_blueprint WHERE grade = $1 FOR KEY SHARE"#,
            source as _
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = locked else {
            return Ok((Some(Early::SourceGone), None));
        };
        source_id = Some(id);
    }
    match pivot {
        Pivot::User(user) => {
            let row = sqlx::query!(
                r#"SELECT role AS "role: Role" FROM app_user WHERE id = $1 FOR NO KEY UPDATE"#,
                user as _
            )
            .fetch_optional(&mut *tx)
            .await?;
            if row.map(|row| row.role) != Some(Role::Student) {
                return Ok((Some(Early::PivotGone), None));
            }
        }
        Pivot::Course(course) => {
            let alive = sqlx::query_scalar!(
                r#"SELECT 1 AS "one" FROM course WHERE id = $1 FOR KEY SHARE"#,
                course as _
            )
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            if !alive {
                return Ok((Some(Early::PivotGone), None));
            }
        }
    }
    // Read off the class row inside the transaction that claims it, so the
    // count this refuses on is the one the pair loop below would iterate.
    let over = match axis.other() {
        Axis::Member => sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM class_group
               WHERE id = $1 AND class_member_count > $2"#,
            class as _,
            Axis::Member.cap(),
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some(),
        Axis::Course => sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM class_group
               WHERE id = $1 AND class_course_count > $2"#,
            class as _,
            Axis::Course.cap(),
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some(),
    };
    Ok((over.then_some(Early::Overloaded), source_id))
}

/// Write the member link and enroll the student into every course the class is
/// already attached to, or write nothing at all.
///
/// A student already enrolled in one of those courses keeps the row they
/// have — no seat is charged, and the existing row's `source` is left exactly
/// as it was, so a hand-placed student is never quietly adopted by a class.
/// A course with no free seat refuses the *whole* join rather than half of it.
///
/// The class counter is claimed by the conditional half of the CTE below
/// rather than a bare increment, so a class deleted out from under this run
/// matches nothing and the whole cascade aborts: [`Attached::Gone`] instead of
/// a counter on a row that no longer exists. The same write carries
/// [`Axis::Member`]'s cap, which is what keeps the pair loop — and therefore
/// this transaction — finite: [`Attached::ClassFull`] once the class is at its
/// ceiling. Claim and insert are one statement, so a refused insert takes its
/// own seat bump back.
pub(crate) async fn add_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    by: &UserId,
) -> Result<Attached<ClassMember>, AppError> {
    let added_at = Timestamp::now();
    let class = class.clone();
    let (user, by) = (*user, *by);
    let outcome = tx_with_retry(db, false, async move |tx| {
        let (early, _) =
            early_verdicts(tx, &class, Axis::Member, Pivot::User(&user), None).await?;
        if let Some(early) = early {
            return Ok(refusal_of(early));
        }
        // The claim and the link are one statement (the cap recipe's CTE): the
        // conditional `UPDATE` on the class row gates the `INSERT`, so a full
        // or gone class writes nothing at all.
        let inserted = match sqlx::query_scalar!(
            r#"WITH seat AS (
                   UPDATE class_group SET class_member_count = class_member_count + 1
                    WHERE id = $1 AND class_member_count < $2
                    RETURNING 1)
               INSERT INTO class_member (class, app_user, added_by, added_at)
               SELECT $1, $3, $4, $5 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING 1 AS "one""#,
            class as _,
            Axis::Member.cap(),
            user as _,
            by as _,
            added_at as _
        )
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(result) => result,
            // A rival landed in the window between the gate and the insert:
            // the link table's own primary key answers, same verdict as the
            // gate, no second seat.
            Err(e) if unique_violation(&e) == Some("class_member_class_user") => {
                return Ok(Attached::Duplicate);
            }
            // The student row vanished mid-run despite the locked pivot claim.
            Err(e) if foreign_key_violation(&e) => return Ok(Attached::PivotGone),
            Err(e) => return Err(e.into()),
        };
        if inserted.is_none() {
            // Full, or the class is gone — the claim matches nothing either
            // way, and only this path pays for the read that tells them apart.
            let standing = sqlx::query_scalar!(
                r#"SELECT 1 AS "one" FROM class_group WHERE id = $1"#,
                class as _
            )
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            return Ok(if standing {
                Attached::ClassFull
            } else {
                Attached::Gone
            });
        }
        let courses: Vec<uuid::Uuid> = sqlx::query_scalar!(
            r#"SELECT course AS "course: uuid::Uuid" FROM class_course WHERE class = $1"#,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?;
        let pairs = courses.into_iter().map(|course| (course, user.uuid()));
        enroll_pairs(tx, &class, pairs, &by).await?;
        Ok(Attached::Made(ClassMember {
            class: class.clone(),
            user,
            added_by: by,
        }))
    })
    .await;
    map_sweep_abort(outcome)
}

/// Write the course link and enroll the class's whole roster into it, or
/// write nothing at all.
///
/// Students already in the course keep the rows they have — no seat charged,
/// `source` untouched — and a roster that does not fit refuses the whole
/// attach rather than filling the course to its cap and stopping.
///
/// `source` is the blueprint whose behalf this attach runs on, and supplying
/// it adds one more claim: that it is still there when the transaction runs
/// ([`Attached::SourceGone`]) — the `FOR KEY SHARE` lock that serializes this
/// transaction against the blueprint's own delete. A hand attach owns itself
/// and passes `None`.
pub(crate) async fn attach_course(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    by: &UserId,
    source: Option<&ClassBlueprintId>,
) -> Result<Attached<ClassCourse>, AppError> {
    let attached_at = Timestamp::now();
    let class = class.clone();
    let course = course.clone();
    let by = *by;
    let source = source.cloned();
    let outcome = tx_with_retry(db, false, async move |tx| {
        let (early, source_id) = early_verdicts(
            tx,
            &class,
            Axis::Course,
            Pivot::Course(&course),
            source.as_ref(),
        )
        .await?;
        if let Some(early) = early {
            return Ok(refusal_of(early));
        }
        let inserted = match sqlx::query_scalar!(
            r#"WITH seat AS (
                   UPDATE class_group SET class_course_count = class_course_count + 1
                    WHERE id = $1 AND class_course_count < $2
                    RETURNING 1)
               INSERT INTO class_course (class, course, attached_by, attached_at, source)
               SELECT $1, $3, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING 1 AS "one""#,
            class as _,
            Axis::Course.cap(),
            course as _,
            by as _,
            attached_at as _,
            source_id,
        )
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(result) => result,
            Err(e) if unique_violation(&e) == Some("class_course_class_course") => {
                return Ok(Attached::Duplicate);
            }
            Err(e) if foreign_key_violation(&e) => return Ok(Attached::PivotGone),
            Err(e) => return Err(e.into()),
        };
        if inserted.is_none() {
            let standing = sqlx::query_scalar!(
                r#"SELECT 1 AS "one" FROM class_group WHERE id = $1"#,
                class as _
            )
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            return Ok(if standing {
                Attached::ClassFull
            } else {
                Attached::Gone
            });
        }
        let members: Vec<uuid::Uuid> = sqlx::query_scalar!(
            r#"SELECT app_user AS "app_user: uuid::Uuid" FROM class_member WHERE class = $1"#,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?;
        let pairs = members.into_iter().map(|user| (course.uuid(), user));
        enroll_pairs(tx, &class, pairs, &by).await?;
        Ok(Attached::Made(ClassCourse {
            class: class.clone(),
            course: course.clone(),
            attached_by: by,
            source: source.clone(),
        }))
    })
    .await;
    map_sweep_abort(outcome)
}

/// Abort markers the pair loop raises where the old transaction `THROW` did,
/// in the [`crate::db::field_update`] style: a refusal found *after* earlier
/// pairs wrote must take the whole run down with it — returning it as `Ok`
/// would COMMIT the partial enrollment. The marker carries the course's key
/// and is mapped back to [`Attached`] right after [`tx_with_retry`] returns
/// ([`map_sweep_abort`]); it never reaches the wire.
const COURSE_GONE_MARK: &str = "class_pump_course_gone:";
const COURSE_FULL_MARK: &str = "class_pump_course_full:";

/// The pair loop's abort markers back into their [`Attached`] refusals —
/// every other outcome passes through untouched.
fn map_sweep_abort<T>(outcome: Result<Attached<T>, AppError>) -> Result<Attached<T>, AppError> {
    match outcome {
        Err(AppError::Internal(m)) if m.starts_with(COURSE_GONE_MARK) => Ok(Attached::CourseGone(
            m[COURSE_GONE_MARK.len()..].to_string(),
        )),
        Err(AppError::Internal(m)) if m.starts_with(COURSE_FULL_MARK) => {
            Ok(Attached::Full(m[COURSE_FULL_MARK.len()..].to_string()))
        }
        other => other,
    }
}

/// Enroll every `(course, user)` pair that has no row yet, each against its
/// own course's capacity, skipping — never charging — the pairs that do.
///
/// Per pair, in the order the refusals outrank each other: the pair's own row
/// answers "already enrolled" (skip, no seat); the course row answers "gone"
/// *before* the seat claim, so a stale link is never mis-reported as a
/// capacity problem; the seat claim is a conditional `UPDATE` on the course
/// row (live cap — `capacity` as it stands at write time, `NULL` reading as
/// unlimited); and the insert rides the seat it claimed. A rival that lands
/// between the gate and the insert answers `23505`, and the bump is given
/// back before the skip — the outcome a re-sent transaction used to reach by
/// seeing the rival's row at its gate.
///
/// Every abort stops the loop inside the caller's transaction: not one of the
/// earlier seats in the same run survives, which is the whole point of doing
/// this in a transaction.
async fn enroll_pairs(
    tx: &mut PgConnection,
    class: &ClassGroupId,
    pairs: impl Iterator<Item = (uuid::Uuid, uuid::Uuid)>,
    by: &UserId,
) -> Result<(), AppError> {
    let now = Timestamp::now();
    for (course, user) in pairs {
        let held = sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM enrollment WHERE course = $1 AND app_user = $2"#,
            course,
            user
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if held {
            continue;
        }
        let alive = sqlx::query_scalar!(r#"SELECT 1 AS "one" FROM course WHERE id = $1"#, course)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !alive {
            return Err(AppError::Internal(format!("{COURSE_GONE_MARK}{course}")));
        }
        let seat = sqlx::query_scalar!(
            r#"UPDATE course SET enrollment_count = enrollment_count + 1
               WHERE id = $1 AND enrollment_count < COALESCE(capacity, $2)
               RETURNING 1 AS "one""#,
            course,
            cap::UNLIMITED,
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !seat {
            return Err(AppError::Internal(format!("{COURSE_FULL_MARK}{course}")));
        }
        let wrote = sqlx::query_scalar!(
            r#"INSERT INTO enrollment (course, app_user, enrolled_by, source, created_at)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (course, app_user) DO NOTHING
               RETURNING 1 AS "one""#,
            course,
            user,
            by as _,
            class as _,
            now.as_millis()
        )
        .fetch_optional(&mut *tx)
        .await;
        match wrote {
            Ok(Some(_)) => {}
            // A rival placed the pair in the window: their row stands, no
            // second seat, and the claim this run took is given back — the
            // skip wins over the capacity answer, exactly as a re-sent
            // transaction found the row at its gate.
            Ok(None) => {
                sqlx::query!(
                    r#"UPDATE course SET enrollment_count = enrollment_count - 1
                       WHERE id = $1"#,
                    course
                )
                .execute(&mut *tx)
                .await?;
            }
            Err(e) if unique_violation(&e) == Some("enrollment_course_user") => {
                sqlx::query!(
                    r#"UPDATE course SET enrollment_count = enrollment_count - 1
                       WHERE id = $1"#,
                    course
                )
                .execute(&mut *tx)
                .await?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// The sweep a role change *off` `student` owes — every class membership (each
// class getting its member count back) and every enrollment row (each course
// getting its seat back) — is two of the arms of
// [`crate::service::user::set_role`], because it belongs in the same
// transaction as the role write that invalidates them. It stays one fact with
// the counters either way: released separately, a failure between the halves
// left enrollment rows tagged `source = class_group:X` while the class had its
// counters back, so the class passed its zero-zero delete guard and the rows
// were left pointing at a class no sweep could ever reach again.

/// Take a student out of a class and sweep the enrollments the class pumped
/// for them. Answers how many link rows went (0 or 1), so the caller can turn
/// zero into a 404.
///
/// The sweep is *repair-first*: an enrollment this class wrote is only deleted
/// once no other class still claims it. Two classes attached to the same
/// course share a student — the second attach skipped the row the first had
/// already written, so the row carries only the first class's name — and
/// deleting it on the first class's way out would unenroll a student the
/// second class is still responsible for. So the row is re-tagged to that
/// rival instead, and only a row nobody is left to claim is deleted and its
/// seat given back. The heir is the lowest class id among the claimants: a
/// deterministic pick (uuid order is mint order), so a repeat of the same
/// sweep lands on the same class.
///
/// That pick is then **claimed** — a `FOR NO KEY UPDATE` read of the heir's
/// class row, no counter moved — before the row is handed over. Both reads
/// behind it are pure, and this sweep is long — one pass per enrollment row
/// the link implies — so `DELETE /classes/{heir}/members/{user}` and
/// `DELETE /classes/{heir}/courses/{course}` could both commit inside it and
/// leave the row tagged with a class holding neither link: a `class_group`
/// whose own 0/0 delete guard then passes, stranding an enrollment nothing can
/// ever sweep. Every one of those writers moves a counter on the heir's own
/// row, so the row lock here puts this transaction on the record they write
/// and the two settle in either order. A claim that matches nothing is an
/// heir whose class row is gone — a stale link outliving its class — and it
/// takes the release arm rather than tagging the row with an id no route can
/// reach.
///
/// Sweeps tolerate rows that are already gone. A course delete wipes a
/// course's enrollments wholesale while the `class_member` rows survive it, so
/// "this class has a member" and "that member has a live pumped row" are
/// independent facts and the loop simply finds nothing to sweep.
pub(crate) async fn remove_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
) -> Result<i64, AppError> {
    let class = class.clone();
    let user = *user;
    tx_with_retry(db, true, async move |tx| {
        let gone = sqlx::query_scalar!(
            r#"DELETE FROM class_member WHERE class = $1 AND app_user = $2
               RETURNING 1 AS "one""#,
            class as _,
            user as _
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(_) = gone else {
            return Ok(0);
        };
        sqlx::query!(
            r#"UPDATE class_group
               SET class_member_count = GREATEST(class_member_count - 1, 0)
               WHERE id = $1"#,
            class as _
        )
        .execute(&mut *tx)
        .await?;
        let rows = sqlx::query!(
            r#"SELECT course AS "course: uuid::Uuid", app_user AS "app_user: uuid::Uuid"
               FROM enrollment WHERE app_user = $1 AND source = $2"#,
            user as _,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| (row.course, row.app_user))
        .collect::<Vec<_>>();
        sweep_enrollments(tx, &class, rows).await?;
        Ok(1)
    })
    .await
}

/// Detach `course` from `class` and sweep the enrollments the class pumped
/// into it. `source`, when given, re-asserts the provenance tag on the link's
/// own delete — a blueprint sweep may only take back rows its own tag owns.
/// Answers how many link rows went, so a caller whose link was a single pair
/// can turn zero into a 404. The sweep underneath is the shared one: a student
/// a second class still claims is re-tagged rather than unenrolled.
pub(crate) async fn detach_course(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    source: Option<&ClassBlueprintId>,
) -> Result<i64, AppError> {
    let class = class.clone();
    let course = course.clone();
    let source = source.cloned();
    tx_with_retry(db, true, async move |tx| {
        let gone = match &source {
            Some(source) => {
                sqlx::query_scalar!(
                    r#"DELETE FROM class_course WHERE class = $1 AND course = $2
                         AND source = (SELECT id FROM class_blueprint WHERE grade = $3)
                       RETURNING 1 AS "one""#,
                    class as _,
                    course as _,
                    source as _
                )
                .fetch_optional(&mut *tx)
                .await?
            }
            None => {
                sqlx::query_scalar!(
                    r#"DELETE FROM class_course WHERE class = $1 AND course = $2
                       RETURNING 1 AS "one""#,
                    class as _,
                    course as _
                )
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        let Some(_) = gone else {
            return Ok(0);
        };
        sqlx::query!(
            r#"UPDATE class_group
               SET class_course_count = GREATEST(class_course_count - 1, 0)
               WHERE id = $1"#,
            class as _
        )
        .execute(&mut *tx)
        .await?;
        let rows = sqlx::query!(
            r#"SELECT course AS "course: uuid::Uuid", app_user AS "app_user: uuid::Uuid"
               FROM enrollment WHERE course = $1 AND source = $2"#,
            course as _,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| (row.course, row.app_user))
        .collect::<Vec<_>>();
        sweep_enrollments(tx, &class, rows).await?;
        Ok(1)
    })
    .await
}

/// The repair-first tail both detaches share: for every enrollment row the
/// deleted link owned, hand it to a rival class that still claims it, or
/// delete it and give the course its seat back.
///
/// The heir is the lowest class id among the rivals that carry both this
/// course *and* the student — the same deterministic pick on every repeat. Its
/// class row is read `FOR NO KEY UPDATE` (a claim that moves no counter: the
/// class_member row already covers the student, and the enrollment is counted
/// on the course) so a concurrent member/course write on the heir serializes
/// behind this transaction; a claim matching nothing is a gone heir, and the
/// row takes the release arm.
async fn sweep_enrollments(
    tx: &mut PgConnection,
    class: &ClassGroupId,
    rows: Vec<(uuid::Uuid, uuid::Uuid)>,
) -> Result<(), AppError> {
    for (course, user) in rows {
        let heir = sqlx::query_scalar!(
            r#"SELECT cm.class AS "class: uuid::Uuid"
               FROM class_member cm
               JOIN class_course cc ON cc.class = cm.class AND cc.course = $1
               WHERE cm.app_user = $2 AND cm.class <> $3
               ORDER BY cm.class
               LIMIT 1"#,
            course,
            user,
            class as _
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(heir) = heir else {
            release(tx, course, user).await?;
            continue;
        };
        let claimed = sqlx::query_scalar!(
            r#"SELECT class_member_count AS "heir_count: i64" FROM class_group
               WHERE id = $1 FOR NO KEY UPDATE"#,
            heir
        )
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_none() {
            release(tx, course, user).await?;
            continue;
        }
        sqlx::query!(
            r#"UPDATE enrollment SET source = $1 WHERE course = $2 AND app_user = $3"#,
            heir,
            course,
            user
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Nobody is left to claim the row: it goes, and its seat with it. The seat
/// is only given back when the delete actually took the row — a course
/// delete's wholesale sweep may have taken both while this transaction read
/// them, and both halves vanish together there.
async fn release(
    tx: &mut PgConnection,
    course: uuid::Uuid,
    user: uuid::Uuid,
) -> Result<(), AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM enrollment WHERE course = $1 AND app_user = $2"#,
        course,
        user
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted > 0 {
        sqlx::query!(
            r#"UPDATE course SET enrollment_count = GREATEST(enrollment_count - 1, 0)
               WHERE id = $1"#,
            course
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    
    use crate::db::class_member::tests::{a_class, counter, fixture_user, rows};
    use crate::domain::class_group::ClassGroupId;
    use crate::error::AppError;
    use crate::service::{class_course, class_member};

    /// The class counter is claimed conditionally, so a class that is not there
    /// stops the cascade before anything is written — rather than leaving a
    /// counter, a link row and a pumped enrollment hanging off a record that
    /// does not exist.
    #[tokio::test]
    async fn an_attach_onto_a_missing_class_writes_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let ghost = ClassGroupId::from_key("019732e3-7b00-7000-8000-00000000dead");

        let refused = class_member::add(
            &db,
            &ghost,
            &fixture_user(&db, "student").await,
            &fixture_user(&db, "manager").await,
        )
        .await;
        assert!(
            matches!(refused, Err(AppError::NotFound)),
            "a class that is gone is a 404, not a counter on nothing: {refused:?}"
        );
        assert_eq!(rows("class_member", &db).await, 0);
        assert_eq!(rows("class_group", &db).await, 0);
    }

    /// The role-change sweep takes a user's whole membership apart, link row
    /// by link row (one transaction per pair since the Postgres port), and
    /// every one of them releases its own class's counter.
    #[tokio::test]
    async fn a_detach_releases_every_class_it_unlinked() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let student = fixture_user(&db, "student").await;
        let classes = [
            a_class("9-A", &db).await,
            a_class("9-B", &db).await,
            a_class("club", &db).await,
        ];
        for class in &classes {
            class_member::add(&db, class, &student, &manager)
                .await
                .unwrap();
        }

        for class in &classes {
            class_member::remove(&db, class, &student).await.unwrap();
        }
        for class in &classes {
            assert_eq!(
                counter("class_member_count", class.uuid(), &db).await,
                0,
                "each class gets its own counter back"
            );
        }

        // And unlinking nobody is a 404, not a silent success — the shape a
        // single-pair detach answers with.
        let again = class_member::remove(&db, &classes[0], &student).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second removal is a 404, not a silent no-op: {again:?}"
        );
    }

    /// The heir a sweep hands a shared enrollment to must still hold *both*
    /// links when the transaction commits — not merely when it read them.
    ///
    /// Two classes carry one course and one student, so the row names the first
    /// and the second is its heir. While that first class's detach sweeps — a
    /// pass per enrollment row, up to a full roster long — the heir drops the
    /// student and detaches the course, both committed. Its two reads see an
    /// heir that is already gone, and its write set (its own class, its own
    /// link, the enrollment) touches nothing the heir's two deletes wrote: with
    /// no read-set conflict detection, both sides commit and the row is left
    /// tagged with a class holding neither link, which then passes its own 0/0
    /// delete guard. The claim on the heir's counter is what puts the two
    /// transactions on one record.
    ///
    /// Real server, and `#[ignore]`d for it, exactly like
    /// [`crate::domain::class_course`]'s course-delete twin: the subject *is*
    /// the store's conflict detection, which `init_mem`'s embedded engine does
    /// not have — it commits both sides and answers `Ok` to each, so this passes
    /// there on broken code.
    ///
    /// The window the old engine needed a schema event to open is the row
    /// lock now: the owner's sweep claims the heir's class row before it
    /// chooses the heir, so the heir's own member/course writes either land
    /// wholly before the sweep reads them or wait behind it — and the
    /// invariant below must hold in both orders. A barrier start lets every
    /// interleaving happen instead of pinning one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_detached_row_is_never_handed_to_a_class_that_let_it_go() {
        use crate::db::class_member::tests::{a_course, fixture_user, source_of};

        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let student = fixture_user(&db, "student").await;
        let (mut raced, mut stranded) = (0, 0);
        for round in 0..4 {
            let algebra = a_course(&format!("algebra{round}"), None, &db).await;
            let owner = a_class(&format!("9-{round}-owner"), &db).await;
            let heir = a_class(&format!("9-{round}-heir"), &db).await;
            for class in [&owner, &heir] {
                class_member::add(&db, class, &student, &manager)
                    .await
                    .unwrap();
                class_course::attach(&db, class, &algebra, &manager)
                    .await
                    .unwrap();
            }
            assert_eq!(
                source_of(&algebra, &student, &db).await,
                Some(Some(owner.clone())),
                "round {round}: the row must start out owned by the first class"
            );

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let detaching = {
                let (db, owner, algebra, gate) =
                    (db.clone(), owner.clone(), algebra.clone(), gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    class_course::detach(&db, &owner, &algebra).await
                })
            };
            // The heir lets the row go, twice over, while the sweep is racing
            // it for the same enrollment row.
            let heir_moves = {
                let (db, heir, algebra, gate) =
                    (db.clone(), heir.clone(), algebra.clone(), gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    class_member::remove(&db, &heir, &student).await?;
                    class_course::detach(&db, &heir, &algebra).await
                })
            };
            let swept = detaching.await.unwrap();
            heir_moves.await.unwrap().unwrap();
            assert!(
                !matches!(swept, Err(AppError::Db(_))),
                "round {round}: a raced detach must be answered, not 500: {swept:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if swept.is_ok() {
                raced += 1;
            }
            if source_of(&algebra, &student, &db).await == Some(Some(heir.clone())) {
                stranded += 1;
            }
        }
        eprintln!("a detach raced by its heir: {raced}/4 rounds swept");
        assert!(raced > 0, "no round ever committed its sweep");
        assert_eq!(
            stranded, 0,
            "an enrollment was left tagged with a class holding neither link"
        );
    }

}
