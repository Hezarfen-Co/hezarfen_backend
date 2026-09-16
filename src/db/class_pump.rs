//! The pump behind the class layer: one link row changed, and every
//! `enrollment` row that link implies reconciled with it, in one transaction.
//!
//! A class section (şube) has two link tables — `class_member` (a student in
//! it) and `class_course` (a course attached to it, as that class×course
//! *instance*) — and the *product* of the two is the roster it owes: every
//! live member is enrolled in every instance the class carries, in real
//! `enrollment` rows tagged [`source`](crate::domain::enrollment) with the
//! class that wrote them. Adding a member and attaching a course are
//! therefore the same operation seen along its two axes, and so are ending
//! one and detaching the other.
//!
//! What is *not* one primitive is attach and detach. They share no statement:
//! one gates on "this pair already holds a row" and claims a counter upwards,
//! the other takes the instance and everything the class taught under it, and
//! the member exits release the enrollment rows the ended stint owned — one
//! row per *instance*, so a section's exit never touches the seat the student
//! holds in another section.
//!
//! Every invariant is one Postgres statement or one row lock, in the shapes
//! [`crate::db::cap`] documents: the class counter is claimed by a conditional
//! `UPDATE` fused with the link's `INSERT` (one CTE — a refused insert takes
//! its own bump back), a duplicate live stint is the partial unique index
//! `class_member_live_pair` answering as `23505` (and enrollment's natural
//! `(class_course, app_user)` key doing the same job through `ON CONFLICT`),
//! a gone parent is a real `FOREIGN KEY` answering as `23503`, and the row
//! locks (`FOR KEY SHARE` / `FOR NO KEY UPDATE` / `FOR UPDATE`) put each
//! transaction on the very record a racing writer must touch. A refusal is an
//! early `Err` (or an `Ok` verdict) from the
//! [`crate::database::tx_with_retry`] closure — a decision, never a retry.

use sqlx::postgres::PgConnection;

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::{Database, foreign_key_violation, tx_with_retry, unique_violation};
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_course::{ClassCourse, ClassCourseId, DersSaati};
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::{ClassMember, ClassMemberId};
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
    /// One of the class's instances no longer exists, named by its id: a
    /// `class_course` row a concurrent detach took while this pump was walking
    /// its pairs. Nothing was written — not one of the earlier rows in the
    /// same run, which is the whole point of doing this in a transaction. No
    /// capacity anyone can raise will change that answer; the attach must be
    /// retried against the class as it now stands.
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
    /// is a third: one of the class's *instances* vanished while the pump was
    /// walking its pairs (a concurrent detach), so the attach must be retried
    /// against the class as it now stands.
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
/// 1. **Duplicate** — "you are already in" outranks every other answer: a
///    caller whose row a rival placed a moment ago must be told they are in,
///    not refused for a class ceiling they are not near. The member axis reads
///    a **live** stint (a left one claims nothing, so rejoining is not a
///    duplicate); the course axis reads the (class, course) pair. (The link
///    tables re-answer this below — `23505` on the live-stint partial index,
///    `ON CONFLICT DO NOTHING` on the enrollment pair — for a rival landing
///    inside the window: same verdict, no second count.)
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
            r#"SELECT 1 AS "one" FROM class_member
               WHERE class = $1 AND app_user = $2 AND left_at IS NULL"#,
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

/// Write the member stint and enroll the student into every instance the
/// class already carries, or write nothing at all.
///
/// A student already enrolled in one of those instances keeps the row they
/// have — no count charged, and the existing row's `source` is left exactly
/// as it was, so a hand-placed student is never quietly adopted by a class.
///
/// The class counter is claimed by the conditional half of the CTE below
/// rather than a bare increment, so a class deleted out from under this run
/// matches nothing and the whole cascade aborts: [`Attached::Gone`] instead of
/// a counter on a row that no longer exists. The same write carries
/// [`Axis::Member`]'s cap, which is what keeps the pair loop — and therefore
/// this transaction — finite: [`Attached::ClassFull`] once the class is at its
/// ceiling. Claim and insert are one statement, so a refused insert takes its
/// own seat bump back.
///
/// The stint is live (`left_at` NULL) by construction, which is the row the
/// partial unique index `class_member_live_pair` guards: a rival landing in
/// the window is answered as [`Attached::Duplicate`] rather than a second
/// live stint for the same pair. A student who left earlier keeps their
/// history row — it claims nothing.
pub(crate) async fn add_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    by: &UserId,
) -> Result<Attached<ClassMember>, AppError> {
    add_member_sourced(db, class, user, by, None).await
}

/// [`add_member`] carrying the stint's *provenance*: the şube the student was
/// copied from, written into `source_class_group` in the same statement as
/// the row. `None` is the hand add — a student someone put here, who came
/// from nowhere (the pump is the only writer of this column, and a rollover
/// is the only caller that has a source to name).
///
/// Everything else — the claim, the cap, the duplicate verdict, the pair loop
/// — is identical, and deliberately so: the tag rides the member axis's own
/// single statement rather than a follow-up write, so a crash cannot leave a
/// copied stint that lost its provenance.
pub(crate) async fn add_member_sourced(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    by: &UserId,
    source: Option<&ClassGroupId>,
) -> Result<Attached<ClassMember>, AppError> {
    let joined_at = Timestamp::now();
    let class = class.clone();
    let source = source.cloned();
    let (user, by) = (*user, *by);
    let id = ClassMemberId::generate();
    let outcome = tx_with_retry(db, false, async move |tx| {
        let (early, _) = early_verdicts(tx, &class, Axis::Member, Pivot::User(&user), None).await?;
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
               INSERT INTO class_member (id, class, app_user, added_by, joined_at, left_at,
                                         source_class_group)
               SELECT $3, $1, $4, $5, $6, NULL, $7 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING 1 AS "one""#,
            class as _,
            Axis::Member.cap(),
            id.uuid(),
            user as _,
            by as _,
            joined_at as _,
            source.as_ref().map(ClassGroupId::uuid),
        )
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(result) => result,
            // A rival landed in the window between the gate and the insert:
            // the live-stint partial index answers, same verdict as the gate,
            // no second seat. (A left row is not in that index, so a rejoin
            // after a leave is never a duplicate.)
            Err(e) if unique_violation(&e) == Some("class_member_live_pair") => {
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
        // The pairs are the class's *instances*, one per attached course: the
        // member axis now enrolls into the class×course instance, not into a
        // school-wide course row.
        let instances: Vec<uuid::Uuid> = sqlx::query_scalar!(
            r#"SELECT id AS "id: uuid::Uuid" FROM class_course WHERE class = $1"#,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?;
        let pairs = instances
            .into_iter()
            .map(|instance| (instance, user.uuid()));
        enroll_pairs(tx, &class, pairs, &by).await?;
        Ok(Attached::Made(ClassMember {
            id: id.clone(),
            class: class.clone(),
            user,
            added_by: by,
            joined_at,
            left_at: None,
            source_class_group: source.clone(),
        }))
    })
    .await;
    map_sweep_abort(outcome)
}

/// Write the instance and enroll the class's whole live roster into it, or
/// write nothing at all.
///
/// Students already enrolled in it keep the rows they have — no count charged,
/// `source` untouched. There is no roster-size gate any more (D5: the
/// counter is a count, not a capacity), so the only refusals left are the
/// class's own ceilings and a vanished pivot.
///
/// `source` is the blueprint whose behalf this attach runs on, and supplying
/// it adds one more claim: that it is still there when the transaction runs
/// ([`Attached::SourceGone`]) — the `FOR KEY SHARE` lock that serializes this
/// transaction against the blueprint's own delete. A hand attach owns itself
/// and passes `None`.
///
/// The instance's own id is minted here (v7, so a class's instances list in
/// creation order) and its policy columns take the schema defaults: one weekly
/// hour, counted toward the karne, an empty roster. A PATCH on `/instances`
/// changes them afterwards.
///
/// Two counters ride the write, both in this one transaction: the class's
/// `class_course_count` (claimed by the conditional CTE, so a full or gone
/// class writes nothing) and the *catalog's* `class_course_count` — how many
/// şubeler teach this course, which is what [`crate::db::course::delete`]'s
/// guard reads. This is the only insert path into `class_course`; every layer
/// above it (hand attach, blueprint pump, rollover copy) reaches the counter
/// through here.
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
    let id = ClassCourseId::generate();
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
               INSERT INTO class_course (id, class, course, attached_by, attached_at, source)
               SELECT $3, $1, $4, $5, $6, $7 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING 1 AS "one""#,
            class as _,
            Axis::Course.cap(),
            id.uuid(),
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
        // The catalog's own counter: how many şubeler teach this course. It is
        // what [`crate::db::course::delete`]'s guard reads — a course a class
        // still teaches may not go — and this insert is the only place it can
        // be claimed. It is a *count*, not a capacity (D5: nothing is refused
        // for being near it), so it is a plain increment in the same
        // transaction as the row: the link and its count commit together, and
        // the early refusals above (a full or gone class, a lost insert) have
        // already returned, so no give-back path exists to get wrong.
        sqlx::query!(
            r#"UPDATE course SET class_course_count = class_course_count + 1 WHERE id = $1"#,
            course as _
        )
        .execute(&mut *tx)
        .await?;
        // The roster is the class's *live* stints: a student who left holds no
        // seat and is enrolled in nothing the class attaches afterwards.
        let members: Vec<uuid::Uuid> = sqlx::query_scalar!(
            r#"SELECT app_user AS "app_user: uuid::Uuid" FROM class_member
               WHERE class = $1 AND left_at IS NULL"#,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?;
        let pairs = members.into_iter().map(|user| (id.uuid(), user));
        enroll_pairs(tx, &class, pairs, &by).await?;
        // The counter the pair loop just moved, read back inside the
        // transaction so the returned row reports the roster it now has
        // rather than the empty one it was inserted with.
        let enrollment_count = sqlx::query_scalar!(
            r#"SELECT enrollment_count AS "count: i64" FROM class_course WHERE id = $1"#,
            id.uuid()
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(Attached::Made(ClassCourse {
            id: id.clone(),
            class: class.clone(),
            course: course.clone(),
            attached_by: by,
            source: source.clone(),
            ders_saati: DersSaati::try_new(crate::constant::MIN_DERS_SAATI)
                .expect("the schema default is a valid weekly-hours count"),
            counts_toward_karne: true,
            enrollment_count,
            attached_at,
        }))
    })
    .await;
    map_sweep_abort(outcome)
}

/// Abort markers the pair loop raises where the old transaction `THROW` did,
/// in the [`crate::db::field_update`] style: a refusal found *after* earlier
/// pairs wrote must take the whole run down with it — returning it as `Ok`
/// would COMMIT the partial enrollment. The marker carries the instance's id
/// and is mapped back to [`Attached`] right after [`tx_with_retry`] returns
/// ([`map_sweep_abort`]); it never reaches the wire.
const COURSE_GONE_MARK: &str = "class_pump_course_gone:";

/// The pair loop's abort markers back into their [`Attached`] refusals —
/// every other outcome passes through untouched.
fn map_sweep_abort<T>(outcome: Result<Attached<T>, AppError>) -> Result<Attached<T>, AppError> {
    match outcome {
        Err(AppError::Internal(m)) if m.starts_with(COURSE_GONE_MARK) => Ok(Attached::CourseGone(
            m[COURSE_GONE_MARK.len()..].to_string(),
        )),
        other => other,
    }
}

/// Enroll every `(instance, user)` pair that has no row yet, and charge the
/// instance's roster counter only for the rows actually written.
///
/// Per pair, in the order the answers outrank each other: the pair's own row
/// answers "already enrolled" (skip, no count moved); the counter claim is a
/// plain increment on the instance row, so it doubles as the existence check —
/// a claim matching nothing is an instance a concurrent detach took, which is
/// the abort marker rather than a refusal of this pair alone; and then the
/// row is inserted with `ON CONFLICT DO NOTHING`, so a rival landing between
/// the gate and the insert leaves *their* row standing and this run gives its
/// own claim back before the skip.
///
/// Every abort stops the loop inside the caller's transaction: not one of the
/// earlier rows in the same run survives, which is the whole point of doing
/// this in a transaction.
async fn enroll_pairs(
    tx: &mut PgConnection,
    class: &ClassGroupId,
    pairs: impl Iterator<Item = (uuid::Uuid, uuid::Uuid)>,
    by: &UserId,
) -> Result<(), AppError> {
    let now = Timestamp::now();
    for (instance, user) in pairs {
        let held = sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM enrollment WHERE class_course = $1 AND app_user = $2"#,
            instance,
            user
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if held {
            continue;
        }
        // The claim doubles as the existence check: there is no capacity to
        // refuse on, so an increment that matches no row means the instance is
        // gone — a stale link, never a full roster.
        let claimed = sqlx::query_scalar!(
            r#"UPDATE class_course SET enrollment_count = enrollment_count + 1
               WHERE id = $1
               RETURNING 1 AS "one""#,
            instance,
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !claimed {
            return Err(AppError::Internal(format!("{COURSE_GONE_MARK}{instance}")));
        }
        let wrote = sqlx::query_scalar!(
            r#"INSERT INTO enrollment (class_course, app_user, enrolled_by, source, created_at)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (class_course, app_user) DO NOTHING
               RETURNING 1 AS "one""#,
            instance,
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
            // second count, and the claim this run took is given back — the
            // skip wins, exactly as a re-sent transaction found the row at its
            // gate.
            Ok(None) => {
                sqlx::query!(
                    r#"UPDATE class_course
                          SET enrollment_count = GREATEST(enrollment_count - 1, 0)
                        WHERE id = $1"#,
                    instance
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
/// for them. Answers how many stints went (0 or 1), so the caller can turn
/// zero into a 404.
///
/// This is the hard form — the row is deleted — and it is kept only for the
/// transfer rollback path, where the stint being undone must leave no trace.
/// The route uses [`leave_member`], which stamps `left_at` and keeps the
/// history.
pub(crate) async fn remove_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
) -> Result<i64, AppError> {
    drop_member(db, class, user, false).await
}

/// The same exit, soft: `left_at` is stamped and the row stays as history.
/// The roster read and the cap both filter on `left_at IS NULL`, so the
/// student holds no seat from this moment — and a rejoin later inserts a
/// fresh live row beside this one (the partial unique index only guards the
/// live stint), which is how "left in October, came back in January" is
/// represented.
///
/// Both exits take the user row `FOR NO KEY UPDATE` first — the same lock the
/// pump's pivot claim and a role-change sweep take — so a demotion cannot
/// interleave with this write in either direction.
pub(crate) async fn leave_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
) -> Result<i64, AppError> {
    drop_member(db, class, user, true).await
}

/// The one body both exits run: find the live stint, end it, give the class
/// its seat back, and release the enrollments the class pumped for the
/// student.
///
/// The sweep's rule, and it is the whole rule: **every row the ended stint
/// owned goes, and each instance gets its roster count back**. An enrollment
/// is keyed `(class_course, app_user)` — one row per *instance* — so a student
/// in two şubeler that teach the same course holds two rows, one on each
/// section's instance, and the seat they keep is the other section's own row,
/// written when they joined it. Handing this row over to a rival class (the
/// pre-remodel rule, when one school-wide course carried one roster) would
/// leave the leaving section's roster, exams and roll-call still listing a
/// student who left it, with its `enrollment_count` inflated on top — a
/// "repair" that repairs nothing, because there was never a shortage of rows
/// to repair.
///
/// Only the rows *this class* wrote are in the list ([`drop_member`] reads
/// `source = <class>`), so a hand-placed enrollment — the operator's own, or
/// one a section handed over before this rule — is never touched.
///
/// Sweeps tolerate rows that are already gone: an instance detach wipes its
/// own enrollments wholesale while the `class_member` rows survive it, so
/// "this class has a member" and "that member has a live pumped row" are
/// independent facts and the loop simply finds nothing to sweep (and, because
/// [`release`] only gives a count back for a row it actually deleted, takes
/// nothing back twice).
async fn drop_member(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    soft: bool,
) -> Result<i64, AppError> {
    let class = class.clone();
    let user = *user;
    let now = Timestamp::now();
    tx_with_retry(db, true, async move |tx| {
        // The role-claim handshake: the same `FOR NO KEY UPDATE` the pump's
        // pivot takes, so a demotion's sweep and this exit serialize on the
        // user row rather than interleaving.
        sqlx::query!(
            r#"SELECT role FROM app_user WHERE id = $1 FOR NO KEY UPDATE"#,
            user as _
        )
        .fetch_optional(&mut *tx)
        .await?;
        let gone = if soft {
            sqlx::query_scalar!(
                r#"UPDATE class_member SET left_at = $3
                   WHERE class = $1 AND app_user = $2 AND left_at IS NULL
                   RETURNING 1 AS "one""#,
                class as _,
                user as _,
                now as _
            )
            .fetch_optional(&mut *tx)
            .await?
        } else {
            sqlx::query_scalar!(
                r#"DELETE FROM class_member
                   WHERE class = $1 AND app_user = $2 AND left_at IS NULL
                   RETURNING 1 AS "one""#,
                class as _,
                user as _
            )
            .fetch_optional(&mut *tx)
            .await?
        };
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
        // Only the rows *this class* wrote: a hand-placed enrollment is the
        // operator's and is never taken back by a class sweep.
        let rows = sqlx::query!(
            r#"SELECT class_course AS "class_course: uuid::Uuid",
                      app_user AS "app_user: uuid::Uuid"
               FROM enrollment WHERE app_user = $1 AND source = $2"#,
            user as _,
            class as _
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| (row.class_course, row.app_user))
        .collect::<Vec<_>>();
        // Every row this class's tag owns goes, and each instance gets its
        // roster count back — see the doc above: the student's seat in a
        // second şube teaching the same course is that şube's *own* row, so
        // there is no heir to hand this one over to.
        for (instance, member) in rows {
            release(tx, instance, member).await?;
        }
        Ok(1)
    })
    .await
}

/// Detach one instance from its class and sweep what hangs off it. `source`,
/// when given, re-asserts the provenance tag on the link's own delete — a
/// blueprint sweep may only take back rows its own tag owns. Answers the blob
/// keys of the image and homework-file rows the sweep removed, or `None` when
/// the pair held no instance at all (nothing written), so a caller whose link
/// was a single pair can turn `None` into a 404 while a detach that removed an
/// instance carrying no uploads still reads as one that went.
///
/// The instance is the anchor now, so detaching it takes everything the class
/// taught under it: its exams (with their results and questions), its sessions
/// with their roll call, its homework with its submissions, its roster and its
/// teacher links. That is the same cascade a course delete runs
/// ([`crate::db::course::sweep_instance_subtree`]), scoped to one instance —
/// and it has to be, because every one of those rows names the instance as its
/// parent key (`ON DELETE NO ACTION`), so deleting the instance while they
/// stand is refused by the store itself.
///
/// The sweep runs behind a `FOR UPDATE` claim on the instance row: a rival
/// detach waits here and finds nothing left to sweep, so exactly one
/// transaction unlinks the pair and gives the class its counter back — and
/// with it the catalog's `class_course_count`, the count
/// [`crate::db::course::delete`]'s guard reads (`course::delete` needs no
/// release of its own: it deletes the course row the counter lives on).
/// This is the only delete path for an instance that does *not* remove the
/// course: the hand detach and the blueprint sweep both come through it.
///
/// The image and homework-file *blobs* the cascade removes come back as keys
/// and are unlinked by the caller, after the commit — the same shape
/// [`crate::service::course::delete`] has. The rows are gone the moment this
/// returns, so a caller that drops the list strands the bytes on disk with
/// nothing left pointing at them.
pub(crate) async fn detach_course(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    source: Option<&ClassBlueprintId>,
) -> Result<Option<Vec<String>>, AppError> {
    let class = class.clone();
    let course = course.clone();
    let source = source.cloned();
    tx_with_retry(db, true, async move |tx| {
        // The claim and the resolution in one read: the row this returns is
        // the very row the lock holds against a rival detach.
        let target = match &source {
            Some(source) => {
                sqlx::query_scalar!(
                    r#"SELECT id AS "id: uuid::Uuid" FROM class_course
                       WHERE class = $1 AND course = $2
                         AND source = (SELECT id FROM class_blueprint WHERE grade = $3)
                       FOR UPDATE"#,
                    class as _,
                    course as _,
                    source as _
                )
                .fetch_optional(&mut *tx)
                .await?
            }
            None => {
                sqlx::query_scalar!(
                    r#"SELECT id AS "id: uuid::Uuid" FROM class_course
                       WHERE class = $1 AND course = $2 FOR UPDATE"#,
                    class as _,
                    course as _
                )
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        let Some(instance) = target else {
            return Ok(None);
        };
        let blobs = crate::db::course::sweep_instance_subtree(&mut *tx, instance).await?;
        sqlx::query!(r#"DELETE FROM class_course WHERE id = $1"#, instance)
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            r#"UPDATE class_group
               SET class_course_count = GREATEST(class_course_count - 1, 0)
               WHERE id = $1"#,
            class as _
        )
        .execute(&mut *tx)
        .await?;
        // …and the catalog's own counter: the course is taught in one fewer
        // şube from this commit. Same transaction as the delete, so the link
        // and its count move together — `course::delete`'s guard reads this,
        // and a course whose last instance was just detached is deletable the
        // moment this lands. `GREATEST` for the same reason the class counter
        // uses it: a drift can only ever be repaired downward, never refused.
        sqlx::query!(
            r#"UPDATE course SET class_course_count = GREATEST(class_course_count - 1, 0)
                WHERE id = $1"#,
            course as _
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(blobs))
    })
    .await
}

/// The row goes, and the instance's roster count with it. It is the *only*
/// fate for an enrollment a section's exit owns: the roster is per instance
/// (D4), so a student who is also in another şube teaching the same course
/// keeps their seat there through that section's own row — see
/// [`drop_member`].
///
/// The count is only given back when the delete actually took the row — an
/// instance detach's wholesale sweep may have taken both while this
/// transaction read them, and both halves vanish together there.
async fn release(
    tx: &mut PgConnection,
    instance: uuid::Uuid,
    user: uuid::Uuid,
) -> Result<(), AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM enrollment WHERE class_course = $1 AND app_user = $2"#,
        instance,
        user
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted > 0 {
        sqlx::query!(
            r#"UPDATE class_course
                  SET enrollment_count = GREATEST(enrollment_count - 1, 0)
                WHERE id = $1"#,
            instance
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
    use crate::service::class_member;

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
}
