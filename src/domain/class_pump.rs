//! The pump behind the class layer: one link row changed, and every
//! `enrollment` row that link implies reconciled with it, in one transaction.
//!
//! A class section (şube) has two link tables — `class_member` (a student in
//! it) and
//! `class_course` (a course attached to it) — and the *product* of the two is
//! the roster it owes: every member is enrolled in every attached course, in
//! real `enrollment` rows tagged [`source`](crate::domain::enrollment) with the
//! class that wrote them. Adding a member and attaching a course are therefore
//! the same operation seen along its two axes, and so are removing one and
//! detaching the other. That axis is what these two primitives take as a
//! parameter, so the four public operations are each a couple of lines.
//!
//! What is *not* one primitive is attach and detach. They share no statement:
//! one gates on "this pair already holds a row" and claims two counters
//! upwards, the other gates on nothing, releases, and has to decide per
//! enrollment row whether a *rival* class still claims it. Folding them behind
//! a direction flag would be one function containing two, so the seam stays
//! where the SQL puts it.
//!
//! Everything here writes counters the way [`crate::domain::cap`] does — a
//! single-record conditional `UPDATE`, never a count-then-write — and every
//! `??` is parenthesized, because `n ?? 0 < $cap` parses as `n ?? (0 < $cap)`
//! and is truthy for every row.

use surrealdb::types::{RecordId, SurrealValue, Value};

use crate::constant::{
    CLASS_COURSE_COUNT_FIELD, CLASS_COURSE_TABLE, CLASS_MEMBER_COUNT_FIELD, CLASS_MEMBER_TABLE,
    ENROLLMENT_COUNT_FIELD, ENROLLMENT_TABLE, MAX_CLASS_COURSES, MAX_CLASS_MEMBERS,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::cap;
use crate::domain::class_group::{ClassGroup, ClassGroupId};
use crate::error::AppError;

/// The `THROW` markers [`attach`] aborts with. `FULL_MARK` and `MISSING_MARK`
/// are *prefixes*: the record id of the course the loop stopped on is appended
/// to them, because neither "the class does not fit" nor "that course is gone"
/// is answerable without naming the course, and the loop only learns which one
/// at runtime. The whole id and not just the key, so the refusal names
/// something a caller can look up without knowing which table it came from.
///
/// `MISSING_MARK` is deliberately not `FULL_MARK`: a seat claim that matches
/// nothing means "full" *or* "no such row", and reporting a class-course link
/// left pointing at a deleted course as a full course sends the caller to raise
/// a capacity that does not exist, on a class every member add now fails on.
///
/// `CAP_MARK` is the class counter's claim matching nothing, which is "the
/// class is full" *or* "the class is gone" — one conditional write cannot say
/// which, so it stays one marker and the read that tells them apart is paid for
/// only on that path (the same shape as
/// [`crate::domain::enrollment::Enrollment::enroll`]). It is not `GONE_MARK`:
/// that one is the *pivot* claim, which is a different row.
///
/// `OVER_MARK` is the *other* axis already standing above its own ceiling —
/// which no claim on this axis can see, and which is what bounds this
/// transaction's write loop (see [`Axis::cap`]). It is its own marker because
/// "this class holds too many courses" is not an answer anyone can act on when
/// it is reported as "this class holds too many students".
///
/// `SOURCE_MARK` is the record that *asked* for this attach — a grade blueprint
/// — being gone by the time the transaction runs. It is the only marker a
/// caller opts into (only a sourced attach states the claim), and it exists
/// because a blueprint's delete sweeps by that tag: a row landing after the
/// sweep would carry the name of a template no sweep can ever reach again.
const HELD_MARK: &str = "class_held";
const GONE_MARK: &str = "class_gone";
const CAP_MARK: &str = "class_cap";
const OVER_MARK: &str = "class_over";
const SOURCE_MARK: &str = "class_no_blueprint";
const FULL_MARK: &str = "class_full:";
const MISSING_MARK: &str = "class_no_course:";

/// An in-transaction existence claim as its own two statements: read `read`
/// into `$name`, and abort with `mark` when it matched nothing.
///
/// Two statements and not one string, because [`attach`] takes the `CREATE`'s
/// result slot off the *length* of its statement list — a pair returned as one
/// element would count as one slot and silently read the wrong result back.
fn claim(name: &str, read: &str, mark: &str) -> Vec<String> {
    vec![
        format!("LET ${name} = ({read})"),
        format!("IF array::len(${name}) = 0 {{ THROW '{mark}' }}"),
    ]
}

/// What [`attach`] settled.
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
    /// are a *class* delete ([`Attached::Gone`] → `class_deleted`, the class
    /// counter's claim matching nothing on a row a re-read no longer finds) and
    /// a *course* delete ([`Attached::PivotGone`] → `course_deleted`, the
    /// course's own claim matching nothing) — and telling a manager the class
    /// vanished when the course did sends them to look at a section that is
    /// standing right there. `linked_course_missing` is a third: *another*
    /// course already attached to this class no longer exists, and it must be
    /// detached before this attach can be retried. `blueprint_deleted` is a
    /// pump losing the template itself mid-run — the only refusal that says
    /// nothing about the (class, course) pair it names.
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
pub(crate) enum Axis {
    /// A student joining: the courses come from `class_course`.
    Member,
    /// A course arriving: the students come from `class_member`.
    Course,
}

impl Axis {
    /// The `SELECT` that yields this run's `(course, user)` pairs, with `$pivot`
    /// bound to the fixed side. In-crate text, never client input.
    fn pairs(&self) -> String {
        match self {
            Axis::Member => format!(
                "SELECT VALUE {{ course: course, user: $pivot }} \
                 FROM {CLASS_COURSE_TABLE} WHERE class = $class"
            ),
            Axis::Course => format!(
                "SELECT VALUE {{ course: $pivot, user: user }} \
                 FROM {CLASS_MEMBER_TABLE} WHERE class = $class"
            ),
        }
    }

    /// The enrollment rows a *detached* link of this axis is responsible for,
    /// as a `WHERE` fragment over `enrollment`. Read off the deleted link row
    /// rather than a bound parameter, so one loop serves a single detach and a
    /// user's whole membership alike.
    fn scope(&self) -> &'static str {
        match self {
            Axis::Member => "user = $link.user",
            Axis::Course => "course = $link.course",
        }
    }

    /// The in-transaction proof that this axis's *pivot* row is still there,
    /// as the statements that claim it.
    ///
    /// The class counter's conditional claim covers the class; nothing covered
    /// the course. With an empty roster the pair loop in [`attach`] touches no
    /// row at all, so a concurrent `DELETE /courses/{id}` conflicts with
    /// nothing — SurrealDB does not conflict-check that (write-skew) — and the
    /// attach commits a `class_course` row pointing at a course that no longer
    /// exists. A conditional write on the course row *is* the check: it matches
    /// nothing once the row is gone, and it puts this transaction on the very
    /// record the course's delete guard writes.
    ///
    /// The counter is re-stated verbatim rather than incremented: the row must
    /// be touched, not changed, and `count = count` leaves an absent counter
    /// absent, so the boot backfill still sees the `NONE` it seeds off.
    ///
    /// The member axis claims nothing: its pivot is a user, whose row is no
    /// class write's to touch, and a membership left pointing at a deleted user
    /// is still removable by its own route — which is exactly what a link to a
    /// deleted course was not.
    fn pivot_claim(&self) -> Vec<String> {
        match self {
            Axis::Member => Vec::new(),
            Axis::Course => claim(
                "alive",
                &format!(
                    "UPDATE $pivot SET {ENROLLMENT_COUNT_FIELD} = \
                     {ENROLLMENT_COUNT_FIELD} RETURN VALUE id"
                ),
                GONE_MARK,
            ),
        }
    }

    /// The class counter this axis's link rows are counted on. Taken off the
    /// axis rather than passed in beside it, because the two are one fact and a
    /// call site that paired the member axis with the course counter would
    /// compile, pass every test — both are `&str` — and desync the class delete
    /// guard forever.
    fn counter(&self) -> &'static str {
        match self {
            Axis::Member => CLASS_MEMBER_COUNT_FIELD,
            Axis::Course => CLASS_COURSE_COUNT_FIELD,
        }
    }

    /// How many link rows this axis's counter may reach. Taken off the axis
    /// beside the counter it bounds, for the same reason.
    ///
    /// This is what makes the pair loop in [`attach`] finite, and it does it
    /// *crosswise*: the member axis's loop iterates the class's `class_course`
    /// rows, which the course axis's counter caps, and the course axis's loop
    /// iterates its `class_member` rows, which the member axis's counter caps.
    /// So bounding the two counters bounds both write loops — one transaction
    /// can never carry more than `MAX_CLASS_MEMBERS`/`MAX_CLASS_COURSES`
    /// enrollment writes.
    ///
    /// Crosswise is also why the claim alone is not enough. It bounds the axis
    /// being *added*, and the loop it runs is the length of the *other* one: a
    /// class that already holds more members than `MAX_CLASS_MEMBERS` — the
    /// layer shipped before either ceiling existed, so a real volume can carry
    /// one — could still have a course attached, and that attach writes one
    /// enrollment per member. So [`attach`] checks the other axis too
    /// ([`Axis::other`]), and the bound holds for stale classes as well.
    fn cap(&self) -> i64 {
        match self {
            Axis::Member => MAX_CLASS_MEMBERS,
            Axis::Course => MAX_CLASS_COURSES,
        }
    }

    /// The refusal code for "the class is *at* this axis's ceiling"
    /// ([`Attached::ClassFull`]), taken off the axis beside the `cap` it
    /// reports, because that ceiling and its name are one fact.
    fn at_ceiling_code(&self) -> &'static str {
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
    fn over_ceiling_code(&self) -> &'static str {
        match self {
            Axis::Member => "class_roster_too_large",
            Axis::Course => "class_course_list_too_large",
        }
    }

    /// The axis whose link rows this one's write loop iterates — its counter is
    /// the length of that loop, and its ceiling is therefore the second half of
    /// the bound.
    fn other(&self) -> Axis {
        match self {
            Axis::Member => Axis::Course,
            Axis::Course => Axis::Member,
        }
    }
}

// The sweep a role change *off* `student` owes — every class membership (each
// class getting its member count back) and every enrollment row (each course
// getting its seat back) — is two of the arms of
// [`crate::domain::user::User::set_role`], because it belongs in the same
// transaction as the role write that invalidates them. It stays one fact with
// the counters either way: released separately, a failure between the halves
// left enrollment rows tagged `source = class_group:X` while the class had its
// counters back, so the class passed its zero-zero delete guard and the rows
// were left pointing at a class no sweep could ever reach again.

/// Write `link` and enroll everything it implies, or write nothing at all.
///
/// The order is the one [`cap::claim_and_create`] settled on and for the same
/// reason: the link is looked for *before* any counter moves, so "you are
/// already in" outranks "there is no room" — a caller whose row a rival placed
/// a moment ago must not be told a course is full about seats they already
/// hold.
///
/// The class counter is claimed by a conditional write rather than a bare
/// increment, so a class deleted out from under this run matches nothing and
/// the whole cascade aborts: [`Attached::Gone`] instead of a counter on a row
/// that no longer exists. The same write carries the axis's [`Axis::cap`],
/// which is what keeps the pair loop below — and therefore this transaction —
/// finite: [`Attached::ClassFull`] once the class is at its ceiling.
///
/// Every enrollment in the loop is *skipped* when the pair already has a row —
/// no seat charged, and the existing row's `source` left exactly as it was, so
/// a hand-placed student is never quietly adopted by a class. Only the pairs
/// that had no row at all are charged, each against its own course's capacity.
///
/// On admissibility ([`crate::database::transaction_with_retry`]): every
/// `CREATE` here is preceded by its own in-transaction existence check, so an
/// "already exists" can only come from a rival that landed inside that window —
/// and re-sending the whole cascade then *sees* the row and takes the other
/// branch. The retry converges instead of re-asking a settled question, which
/// is the restriction's actual test.
///
/// `source` is the record whose behalf this attach runs on — a grade blueprint
/// — and supplying it adds one more claim: that it is still there when the
/// transaction runs ([`Attached::SourceGone`]). A hand attach owns itself and
/// passes `None`.
pub(crate) async fn attach<T: SurrealValue + Clone>(
    class: &ClassGroupId,
    axis: Axis,
    link: &RecordId,
    row: &T,
    pivot: RecordId,
    by: RecordId,
    source: Option<RecordId>,
    db: &Database,
) -> Result<Attached<T>, AppError> {
    let count_field = axis.counter();
    let pairs = axis.pairs();
    let other = axis.other();
    let over_field = other.counter();
    // One statement per element, because the `CREATE`'s result slot is this
    // list's own length at the moment it is pushed. It used to be a
    // hand-counted constant with a case per optional claim, and only the path
    // carrying that claim would ever have paid for a miscount.
    let mut statements = vec![
        "BEGIN TRANSACTION".to_string(),
        "LET $held = (SELECT VALUE id FROM $link)".to_string(),
        format!("IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }}"),
    ];
    if source.is_some() {
        statements.extend(claim("source", "SELECT VALUE id FROM $guard", SOURCE_MARK));
    }
    statements.extend(axis.pivot_claim());
    // Read off the class row inside the transaction that claims it, so the
    // count this refuses on is the one the pair loop below would iterate.
    statements.push(format!(
        "LET $over = (SELECT VALUE id FROM $class WHERE ({over_field} ?? 0) > $other_cap)"
    ));
    statements.push(format!(
        "IF array::len($over) > 0 {{ THROW '{OVER_MARK}' }}"
    ));
    statements.push(format!(
        "LET $counted = (UPDATE $class SET {count_field} = ({count_field} ?? 0) + 1 \
         WHERE ({count_field} ?? 0) < $class_cap RETURN VALUE id)"
    ));
    statements.push(format!(
        "IF array::len($counted) = 0 {{ THROW '{CAP_MARK}' }}"
    ));
    // Taken as it is pushed: this is the slot the link row comes back out of.
    let made = statements.len();
    statements.push("CREATE $link CONTENT $row".to_string());
    statements.push(format!(
        "FOR $pair IN (({pairs}) ?? []) {{
             LET $seat_of = type::record('{ENROLLMENT_TABLE}', string::concat(
                 record::id($pair.course), '_', record::id($pair.user)));
             IF array::len((SELECT VALUE id FROM $seat_of)) = 0 {{
                 IF array::len((SELECT VALUE id FROM $pair.course)) = 0 {{
                     THROW '{MISSING_MARK}' + <string>$pair.course
                 }};
                 LET $seat = (UPDATE $pair.course SET {ENROLLMENT_COUNT_FIELD} = \
                     ({ENROLLMENT_COUNT_FIELD} ?? 0) + 1 \
                     WHERE ({ENROLLMENT_COUNT_FIELD} ?? 0) < (capacity ?? $unlimited) \
                     RETURN VALUE id);
                 IF array::len($seat) = 0 {{
                     THROW '{FULL_MARK}' + <string>$pair.course
                 }};
                 CREATE $seat_of CONTENT {{ course: $pair.course, user: $pair.user, \
                     enrolled_by: $by, source: $class }};
             }};
         }}"
    ));
    statements.push("COMMIT TRANSACTION".to_string());
    let sql = format!("{};", statements.join(";\n"));
    let mut bindings = vec![
        ("class".into(), class.record().into_value()),
        ("link".into(), link.clone().into_value()),
        ("row".into(), row.clone().into_value()),
        ("pivot".into(), pivot.into_value()),
        ("by".into(), by.into_value()),
        ("unlimited".into(), cap::UNLIMITED.into_value()),
        ("class_cap".into(), axis.cap().into_value()),
        ("other_cap".into(), other.cap().into_value()),
    ];
    if let Some(guard) = source {
        bindings.push(("guard".into(), guard.into_value()));
    }
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &bindings,
        &[
            HELD_MARK,
            GONE_MARK,
            OVER_MARK,
            SOURCE_MARK,
            CAP_MARK,
            FULL_MARK,
            MISSING_MARK,
        ],
    )
    .await?;
    // "Already linked" is read first: it outranks both refusals below, and the
    // gate aborts before either could fire.
    //
    // Only the `THROW` is read, never an `is_already_exists` off the `CREATE`
    // itself. A rival landing in the window between the gate and the create
    // makes the store answer "already exists", which
    // [`transaction_with_retry`] treats as a lost round and re-sends — and that
    // re-send is the right answer, because the second pass *sees* the row at
    // the gate. Reading it here would also mis-file the enrollment `CREATE`'s
    // version of the same answer as "already in this class".
    if errors
        .values()
        .any(|error| error.to_string().contains(HELD_MARK))
    {
        return Ok(Attached::Duplicate);
    }
    // The blueprint that asked for this attach is gone, and that outranks every
    // refusal below it: none of them is an answer anyone can act on once the
    // template that wanted the row has been deleted — and its claim stands
    // first in the transaction, so it aborts before they could fire anyway.
    if errors
        .values()
        .any(|error| error.to_string().contains(SOURCE_MARK))
    {
        return Ok(Attached::SourceGone);
    }
    if errors
        .values()
        .any(|error| error.to_string().contains(GONE_MARK))
    {
        return Ok(Attached::PivotGone);
    }
    // Read before the ceiling below: a class over the *other* axis's ceiling is
    // refused whether or not this axis has room, and being told it is full on
    // an axis with places left is an answer nobody can act on.
    if errors
        .values()
        .any(|error| error.to_string().contains(OVER_MARK))
    {
        return Ok(Attached::ClassOverloaded);
    }
    // Full, or the class is gone — the counter claim matches nothing either
    // way, and only this path pays for the read that tells them apart.
    if errors
        .values()
        .any(|error| error.to_string().contains(CAP_MARK))
    {
        return Ok(match ClassGroup::read(class, db).await? {
            Some(_) => Attached::ClassFull,
            None => Attached::Gone,
        });
    }
    // The stale link is read before "full": both come out of the same seat
    // claim matching nothing, and only one of them is a capacity problem.
    if let Some(course) = errors
        .values()
        .find_map(|error| named_course(&error.to_string(), MISSING_MARK))
    {
        return Ok(Attached::CourseGone(course));
    }
    if let Some(course) = errors
        .values()
        .find_map(|error| named_course(&error.to_string(), FULL_MARK))
    {
        return Ok(Attached::Full(course));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    result
        .take::<Vec<T>>(made)?
        .into_iter()
        .next()
        .map(Attached::Made)
        .ok_or_else(|| AppError::Internal("the class pump wrote no link row".into()))
}

/// The course id out of a `<mark><table>:<key>` abort. Table names and the
/// generated ULID keys here are alphanumeric and `:` joins them, so the id ends
/// where the store's own wrapping around the thrown text begins.
fn named_course(message: &str, mark: &str) -> Option<String> {
    let id: String = message
        .split_once(mark)?
        .1
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Drop the link rows `links` names, give each class its counter back, and
/// sweep the enrollment rows those links pumped. Answers how many link rows
/// went, so a caller whose link was a single pair can turn zero into a 404.
///
/// The sweep is *repair-first*: an enrollment this class wrote is only deleted
/// once no other class still claims it. Two classes attached to the same course
/// share a student — the second attach skipped the row the first had already
/// written, so the row carries only the first class's name — and deleting it on
/// the first class's way out would unenroll a student the second class is still
/// responsible for. So the row is re-tagged to that rival instead, and only a
/// row nobody is left to claim is deleted and its seat given back. The heir is
/// the lowest class id among the claimants: a deterministic pick, so a repeat
/// of the same sweep lands on the same class.
///
/// Sweeps tolerate rows that are already gone. `Course::delete` wipes a
/// course's enrollments wholesale while the `class_member` rows survive it, so
/// "this class has a member" and "that member has a live pumped row" are
/// independent facts and the loop simply finds nothing to sweep.
///
/// Admissible by construction: `DELETE` and `UPDATE` only, so no statement in
/// the cascade can answer "already exists" and every lost round is a plain
/// re-send.
pub(crate) async fn detach(
    links: &str,
    axis: Axis,
    bindings: &[(String, Value)],
    db: &Database,
) -> Result<i64, AppError> {
    let count_field = axis.counter();
    let scope = axis.scope();
    let sweep = format!(
        "FOR $row IN ((SELECT id, course, user FROM {ENROLLMENT_TABLE} \
                 WHERE {scope} AND source = $link.class) ?? []) {{
                 LET $rivals = (SELECT VALUE class FROM {CLASS_COURSE_TABLE} \
                     WHERE course = $row.course AND class != $link.class);
                 LET $heir = array::first(array::sort((SELECT VALUE class \
                     FROM {CLASS_MEMBER_TABLE} WHERE user = $row.user AND class IN $rivals)));
                 IF $heir != NONE {{
                     UPDATE $row.id SET source = $heir;
                 }} ELSE {{
                     DELETE $row.id;
                     UPDATE $row.course SET {ENROLLMENT_COUNT_FIELD} = \
                         math::max([({ENROLLMENT_COUNT_FIELD} ?? 0) - 1, 0]);
                 }};
             }};"
    );
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $gone = (DELETE {links} RETURN BEFORE);
         FOR $link IN ($gone ?? []) {{
             UPDATE $link.class SET {count_field} = math::max([({count_field} ?? 0) - 1, 0]);
             {sweep}
         }};
         RETURN array::len($gone);
         COMMIT TRANSACTION;"
    );
    let (mut result, mut errors) = transaction_with_retry(db, &sql, bindings, &[]).await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the LET and the FOR: the RETURN is slot 3.
    Ok(result.take::<Vec<i64>>(3)?.into_iter().next().unwrap_or(0))
}

/// A composite id for a `(class, other)` link, the shape
/// [`crate::domain::enrollment::EnrollmentId::composite`] uses: the same pair
/// always maps to the same record, so one row per pair holds by construction
/// and a duplicate is something the store can *see* rather than something a
/// find-then-insert has to race.
pub(crate) fn link_id(table: &str, class: &ClassGroupId, other: &str) -> RecordId {
    RecordId::new(table, format!("{}_{}", class.key(), other))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{CLASS_MEMBER_COUNT_FIELD, CLASS_MEMBER_TABLE};
    use crate::domain::class_group::ClassGroupId;
    use crate::domain::class_member::ClassMember;
    use crate::domain::class_member::tests::{a_class, counter, rows};
    use crate::domain::user::UserId;
    use crate::error::AppError;

    /// The class counter is claimed conditionally, so a class that is not there
    /// stops the cascade before anything is written — rather than leaving a
    /// counter, a link row and a pumped enrollment hanging off a record that
    /// does not exist.
    #[tokio::test]
    async fn an_attach_onto_a_missing_class_writes_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let ghost = ClassGroupId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");

        let refused = ClassMember::add(
            &ghost,
            &UserId::from_key("student"),
            &UserId::from_key("manager"),
            &db,
        )
        .await;
        assert!(
            matches!(refused, Err(AppError::NotFound)),
            "a class that is gone is a 404, not a counter on nothing: {refused:?}"
        );
        assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
        assert_eq!(rows("SELECT VALUE id FROM class_group", &db).await, 0);
    }

    /// [`detach`] is written to take *many* link rows at once — that is what
    /// makes a user's whole membership one statement — and every one of them
    /// releases its own class's counter. Called straight, because the public
    /// `delete_for_user` throws the count away.
    #[tokio::test]
    async fn a_detach_releases_every_class_it_unlinked() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let classes = [
            a_class("9-A", &db).await,
            a_class("9-B", &db).await,
            a_class("club", &db).await,
        ];
        for class in &classes {
            ClassMember::add(class, &student, &manager, &db)
                .await
                .unwrap();
        }

        let gone = detach(
            &format!("{CLASS_MEMBER_TABLE} WHERE user = $usr"),
            Axis::Member,
            &[("usr".into(), student.record().into_value())],
            &db,
        )
        .await
        .unwrap();
        assert_eq!(
            gone, 3,
            "every link row must be counted, not just the first"
        );
        for class in &classes {
            assert_eq!(
                counter(CLASS_MEMBER_COUNT_FIELD, class.record(), &db).await,
                0,
                "each class gets its own counter back"
            );
        }

        // And a run that unlinks nothing answers zero, which is what turns a
        // single-pair detach into a 404 instead of a silent success.
        let again = detach(
            &format!("{CLASS_MEMBER_TABLE} WHERE user = $usr"),
            Axis::Member,
            &[("usr".into(), student.record().into_value())],
            &db,
        )
        .await
        .unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn a_full_abort_names_its_course() {
        assert_eq!(
            named_course("An error occurred: class_full:course:01J8XZ0K3Q", FULL_MARK),
            Some("course:01J8XZ0K3Q".to_string())
        );
        assert_eq!(
            named_course("class_full:course:algebra'", FULL_MARK),
            Some("course:algebra".into())
        );
        assert_eq!(named_course("class_held", FULL_MARK), None);
        assert_eq!(named_course("class_full:", FULL_MARK), None);
        // And the stale-link abort reads off the same shape, without the two
        // markers ever matching each other's text.
        assert_eq!(
            named_course("class_no_course:course:algebra'", MISSING_MARK),
            Some("course:algebra".into())
        );
        assert_eq!(named_course("class_no_course:course:a", FULL_MARK), None);
        assert_eq!(named_course("class_full:course:a", MISSING_MARK), None);
    }
}
