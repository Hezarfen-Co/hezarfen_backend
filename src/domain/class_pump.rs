//! The pump behind the class layer: one link row changed, and every
//! `enrollment` row that link implies reconciled with it, in one transaction.
//!
//! A class (şube) has two link tables — `class_member` (a student in it) and
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
    ENROLLMENT_COUNT_FIELD, ENROLLMENT_TABLE,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::cap;
use crate::domain::class_group::ClassGroupId;
use crate::error::AppError;

/// The `THROW` markers [`attach`] aborts with. `FULL_MARK` is a *prefix*: the
/// record id of the course that had no seat is appended to it, because "the
/// class does not fit" is unanswerable without knowing which course to raise
/// the capacity of, and the loop only learns that at runtime. The whole id and
/// not just the key, so the refusal names something a caller can look up
/// without knowing which table it came from.
const HELD_MARK: &str = "class_held";
const GONE_MARK: &str = "class_gone";
const FULL_MARK: &str = "class_full:";

/// What [`attach`] settled.
pub(crate) enum Attached<T> {
    /// The link row, the counter and every enrollment it implied committed
    /// together.
    Made(T),
    /// The link already exists — nothing was written, and no seat was spent
    /// finding that out.
    Duplicate,
    /// The class row is gone (a concurrent delete won). Nothing was written.
    Gone,
    /// One of the courses had no free seat, named by its key. Nothing was
    /// written — not one of the earlier seats in the same run, which is the
    /// whole point of doing this in a transaction.
    Full(String),
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
}

/// Whether a [`detach`] also reconciles the enrollment rows its links pumped.
pub(crate) enum Sweep {
    /// Repair or delete them, per the rule in [`detach`].
    Rows,
    /// Release the counters and touch nothing else, because something else owns
    /// the enrollment side of this same event.
    CounterOnly,
}

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
/// that no longer exists.
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
pub(crate) async fn attach<T: SurrealValue + Clone>(
    class: &ClassGroupId,
    axis: Axis,
    link: &RecordId,
    row: &T,
    pivot: RecordId,
    by: RecordId,
    db: &Database,
) -> Result<Attached<T>, AppError> {
    let count_field = axis.counter();
    let pairs = axis.pairs();
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $link);
         IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }};
         LET $counted = (UPDATE $class SET {count_field} = ({count_field} ?? 0) + 1 \
             WHERE ({count_field} ?? 0) < $unlimited RETURN VALUE id);
         IF array::len($counted) = 0 {{ THROW '{GONE_MARK}' }};
         CREATE $link CONTENT $row;
         FOR $pair IN (({pairs}) ?? []) {{
             LET $seat_of = type::record('{ENROLLMENT_TABLE}', string::concat(
                 record::id($pair.course), '_', record::id($pair.user)));
             IF array::len((SELECT VALUE id FROM $seat_of)) = 0 {{
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
         }};
         COMMIT TRANSACTION;"
    );
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[
            ("class".into(), class.record().into_value()),
            ("link".into(), link.clone().into_value()),
            ("row".into(), row.clone().into_value()),
            ("pivot".into(), pivot.into_value()),
            ("by".into(), by.into_value()),
            ("unlimited".into(), cap::UNLIMITED.into_value()),
        ],
        &[HELD_MARK, GONE_MARK, FULL_MARK],
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
    if errors
        .values()
        .any(|error| error.to_string().contains(GONE_MARK))
    {
        return Ok(Attached::Gone);
    }
    if let Some(course) = errors
        .values()
        .find_map(|error| full_course(&error.to_string()))
    {
        return Ok(Attached::Full(course));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, two LETs and two IFs: the CREATE is slot 5.
    result
        .take::<Vec<T>>(5)?
        .into_iter()
        .next()
        .map(Attached::Made)
        .ok_or_else(|| AppError::Internal("the class pump wrote no link row".into()))
}

/// The course id out of a `class_full:<table>:<key>` abort. Table names and the
/// generated ULID keys here are alphanumeric and `:` joins them, so the id ends
/// where the store's own wrapping around the thrown text begins.
fn full_course(message: &str) -> Option<String> {
    let id: String = message
        .split_once(FULL_MARK)?
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
/// [`Sweep::CounterOnly`] is the deliberate half of this: dropping a *user's*
/// whole membership releases the counters and nothing else, because
/// `Enrollment::delete_for_user` owns the enrollment side of that same event and
/// a second decrement of the same seat is a bug, not a belt.
///
/// Admissible by construction: `DELETE` and `UPDATE` only, so no statement in
/// the cascade can answer "already exists" and every lost round is a plain
/// re-send.
pub(crate) async fn detach(
    links: &str,
    axis: Axis,
    sweep: Sweep,
    bindings: &[(String, Value)],
    db: &Database,
) -> Result<i64, AppError> {
    let count_field = axis.counter();
    let sweep = match sweep {
        Sweep::CounterOnly => String::new(),
        Sweep::Rows => {
            let scope = axis.scope();
            format!(
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
            )
        }
    };
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
            Sweep::CounterOnly,
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
            Sweep::CounterOnly,
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
            full_course("An error occurred: class_full:course:01J8XZ0K3Q"),
            Some("course:01J8XZ0K3Q".to_string())
        );
        assert_eq!(
            full_course("class_full:course:algebra'"),
            Some("course:algebra".into())
        );
        assert_eq!(full_course("class_held"), None);
        assert_eq!(full_course("class_full:"), None);
    }
}
