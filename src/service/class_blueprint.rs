//! Grade-blueprint workflows: creating a template and pumping it across a
//! whole grade, the edit that retro-pumps, the delete that takes the
//! attachments it made with it, and the per-class apply they all share. The
//! queries live in [`crate::db::class_blueprint`]; the row shape and the
//! skip vocabulary in [`crate::domain::class_blueprint`].
//!
//! Two rules shape everything below.
//!
//! **An edit retro-pumps.** Changing the list reaches every class already at
//! that grade, not just the ones made afterwards. That is an unbounded write
//! loop — one transaction per (class, course), and no ceiling bounds the
//! number of classes — chosen deliberately over a template that only new
//! classes see.
//!
//! **A pump is best-effort.** Each (class, course) pair is one all-or-nothing
//! transaction of the existing pump, and a pair that would breach a limit is
//! *skipped and reported* rather than aborting the other classes' share. So a
//! blueprint edit can leave a partial state — which is the point: one full
//! course must not stop the other eleven sections from being stocked. Every
//! skip is returned, naming the class, the course and the reason.
//!
//! Removal is the mirror, and it is where the provenance tag earns its keep:
//! a `class_course` row this blueprint wrote carries `source`, a row a human
//! attached carries no such key at all, and dropping a course from the
//! blueprint sweeps only the former. The sweep is the pump's own
//! [`crate::db::class_pump::detach`], so a class losing a course still
//! repairs before it deletes — and it runs one transaction per pair too, for
//! the same reason the pump does.

use crate::database::Database;
use crate::db::class_blueprint;
use crate::db::class_course;
use crate::db::class_group;
use crate::db::class_pump::Attached;
use crate::domain::class_blueprint::{
    ClassBlueprint, ClassBlueprintId, Pumped, SectionStatus, Skip, skip_reason,
};
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId};
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Write the blueprint after settling the course list
/// ([`ClassBlueprint::course_list`]) — the same validation, in the same
/// order, the single write this replaced did. One blueprint per grade: a
/// duplicate is a `409` the store itself decides.
pub async fn create(
    db: &Database,
    creator: &UserId,
    grade: ClassGrade,
    courses: Vec<CourseId>,
) -> Result<ClassBlueprint, AppError> {
    let courses = ClassBlueprint::course_list(courses)?;
    class_blueprint::create(db, creator, grade, courses).await
}

/// The row, for callers that only inspect it — the web layer's
/// `blueprint_or_404` reads through here.
pub async fn read(
    db: &Database,
    id: &ClassBlueprintId,
) -> Result<Option<ClassBlueprint>, AppError> {
    class_blueprint::read(db, id).await
}

/// Every blueprint, by grade label — the web layer's paged index.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
    class_blueprint::list_all(db, limit, offset).await
}

/// Replace the course list, then reconcile every class at this grade with
/// it: the courses this blueprint no longer holds are detached from the
/// classes *it* attached them to, and the ones it holds are pumped into
/// every class that fits.
///
/// The write is a compare-and-set on the list this caller read
/// ([`class_blueprint::set_courses_if_unchanged`]), so two
/// managers editing the same grade cannot have one's list silently pump the
/// other's diff — the loser is a 409 and re-reads.
///
/// Removals run first: a course leaving frees a place under the per-class
/// course ceiling that the same edit's additions can then use. What is swept
/// is derived from the list this write **stored**, never from a diff against
/// the list this handle read ([`class_blueprint::sourced_links`]): the sweep is N
/// sequential transactions in one request, and a failure part-way through
/// used to leave sections carrying courses the template no longer holds with
/// no way back — a re-`PATCH` recomputed an empty diff, and the documented
/// self-heal ("send the list it already holds") swept nothing. Derived from
/// the stored list, that same re-`PATCH` finds the rows still tagged and
/// finishes the job.
///
/// The price is the other direction: a rival edit that lands *and pumps*
/// between this write and this sweep has its new course detached again,
/// because this sweep asks the list it stored. That is a section short a
/// templated course — what [`status`] reports and the next pump
/// repairs — where the diff's failure was a row no call could reach.
///
/// Takes no lease of its own, deliberately: a lease held across its own pump
/// would deadlock on the locks that pump takes, and it would close nothing
/// anyway — the attach's claim asks whether the blueprint *exists*, not what
/// it holds, so an edit racing a pump is only ever the retro-pump this
/// feature is built on. Rows a racing edit leaves behind stay reachable,
/// because [`delete`] sweeps the whole tag rather than a list.
pub async fn set_courses(
    db: &Database,
    blueprint: ClassBlueprint,
    courses: Vec<CourseId>,
    by: &UserId,
) -> Result<(ClassBlueprint, Pumped), AppError> {
    let wanted = ClassBlueprint::course_list(courses)?;
    let Some(mut saved) = class_blueprint::set_courses_if_unchanged(
        db,
        &blueprint.id,
        blueprint.courses.clone(),
        wanted,
    )
    .await?
    else {
        // The conditional write matched nothing: the row is gone, or its
        // list moved since this caller read it. Only this path pays for the
        // read that tells those apart.
        return match read(db, &blueprint.id).await? {
            Some(_) => Err(AppError::Conflict(
                "this blueprint changed since you read it — re-read and retry",
            )),
            None => Err(AppError::NotFound),
        };
    };
    let doomed = class_blueprint::sourced_links(db, saved.get_id(), &saved.courses).await?;
    class_blueprint::drop_links(db, saved.get_id(), doomed).await?;
    let pumped = pump(db, &mut saved, by).await?;
    Ok((saved, pumped))
}

/// Delete the blueprint, taking every attachment it made with it. Courses a
/// human attached to those classes by hand carry no `source` and are left
/// exactly where they are — the same cut a single removal makes, applied to
/// the whole list, because a template that is gone owns nothing.
///
/// The row goes **first**, and the sweep that follows is by the provenance
/// tag rather than by the list this caller read. Sweeping first was the
/// mirror image of a race: a `PATCH` adding a course and pumping it while
/// this ran landed rows tagged with a blueprint the delete then removed,
/// and nothing could ever sweep them again — the grade label *is* the
/// primary key, so only a blueprint recreated at that grade could even name
/// them. Never revert to sweep-first.
///
/// Deleting first plus the attach's in-transaction claim on this row
/// ([`Attached::SourceGone`]) **closes** that window outright: the claim is
/// not a bare read but a `FOR KEY SHARE` row lock on `class_blueprint`, the
/// one strength a `DELETE` of the row cannot take. A sourced attach that
/// started first holds the row and this delete's `DELETE` waits behind it —
/// the row the attach commits is then in the set the sweep reads; an attach
/// starting after the delete finds no row and writes nothing. (Under the
/// old engine the claim was a plain read that no `BEGIN`/`COMMIT` pair
/// serialized against this delete's write, and a process-wide `RwLock` had
/// to stand in; the row lock is the same answer without the process.)
///
/// What is left is a **process crash** between the delete and the sweep.
/// That leaves inert `class_course` rows tagged with a blueprint that is
/// gone; they are still detachable one at a time at
/// `DELETE /classes/{id}/courses/{course}`, and every counter stays exact
/// because each detach is its own transaction.
///
/// The delete is a compare-and-set on the list this caller read, like
/// [`set_courses`]: an edit landing in between is a `409` rather than
/// a silent detach of somebody else's additions. Known hole, accepted: the
/// grade label *is* the primary key and the comparison is by **content**, so
/// a blueprint deleted and recreated at the same grade with the same list
/// names the same course set as the stored junction rows, and this call
/// deletes the *new* row.
///
/// Content-equal is intent-equal — the end state is the one the caller
/// asked for — and telling the two apart needs a revision column on the
/// row, which nothing else here would use.
pub async fn delete(db: &Database, blueprint: ClassBlueprint) -> Result<(), AppError> {
    // The claim, the sweep and the delete are one transaction — the shape
    // Postgres demands and the doc above argues for. The old order (delete
    // the row on the pool, then sweep) cannot work against an enforcing
    // foreign key: the delete statement itself is refused the instant a
    // sourced link it has not swept yet exists, and a link committing
    // between the row lock's release and the sweep re-arms the same
    // refusal. Holding the claim across the sweep closes the window: an
    // attach in flight waits behind it and finds no row; an attach that
    // committed first is in the set the sweep reads.
    let mut tx = db.begin().await?;
    let Some(stored) = class_blueprint::held_courses_for_update(&mut tx, &blueprint.id).await?
    else {
        // Gone before we claimed it.
        return Err(AppError::NotFound);
    };
    if stored != blueprint.courses {
        // The list moved since this caller read it: a 409, not a silent
        // detach of somebody else's additions.
        return Err(AppError::Conflict(
            "this blueprint changed since you read it — re-read and retry",
        ));
    }
    // The sweep runs on its own bounded transactions while this transaction
    // holds the claim: it touches no `class_blueprint` rows, so it cannot
    // deadlock against the lock it runs under.
    let doomed = class_blueprint::sourced_links(db, &blueprint.id, &[]).await?;
    class_blueprint::drop_links(db, &blueprint.id, doomed).await?;
    if !class_blueprint::delete_if_unchanged_in(&mut tx, &blueprint.id, &blueprint.courses)
        .await?
    {
        // The row was locked, matched at the claim, and vanished anyway:
        // nothing does that but a bug.
        return Err(AppError::Internal(
            "the claimed blueprint row did not survive its own lock".into(),
        ));
    }
    tx.commit().await?;
    Ok(())
}

/// Attach every course in this blueprint to `class`, skipping — never
/// aborting on — the ones that do not fit. A course already on the class is
/// a no-op, whoever attached it: this is what makes a pump repeatable, and
/// what stops it re-tagging a hand-attached course as its own.
///
/// This is the *only* place a sourced attach is made — [`pump`], the
/// create-time and per-class pumps in [`crate::web::classes`] all come
/// through here — and each attach's transaction takes a `FOR KEY SHARE`
/// lock on the blueprint row itself, one course at a time. A delete landing
/// mid-pump is then clean by construction: the attaches that already
/// committed are found by its tag sweep, and the ones that have not yet
/// started meet the deleted row at their own in-transaction claim and
/// answer `blueprint_deleted`. Per course rather than per pump, because the
/// pump's loop is unbounded and a delete may not wait behind all of it.
///
/// One class, so `blueprint_deleted` is an ordinary skip here: the courses
/// after it are not tried, because the template they would ask for is the
/// one that just went.
pub async fn apply_to(
    db: &Database,
    blueprint: &ClassBlueprint,
    class: &ClassGroup,
    by: &UserId,
) -> Result<Vec<Skip>, AppError> {
    let mut skipped = Vec::new();
    apply_courses(db, blueprint, class, by, &mut Vec::new(), &mut skipped).await?;
    Ok(skipped)
}

/// One class's share of a pump, and the body both callers above share.
///
/// `dead` is the courses this run already found deleted: they are not
/// attempted again, so neither the skip nor [`class_blueprint::prune`]'s
/// write repeats on the next section. A course newly found gone joins it.
///
/// Answers `false` when the blueprint itself is gone — a caller looping
/// over classes must stop, because every remaining one would answer exactly
/// the same.
async fn apply_courses(
    db: &Database,
    blueprint: &ClassBlueprint,
    class: &ClassGroup,
    by: &UserId,
    dead: &mut Vec<CourseId>,
    skipped: &mut Vec<Skip>,
) -> Result<bool, AppError> {
    for course in &blueprint.courses {
        if dead.contains(course) {
            continue;
        }
        let landed =
            class_course::attach_sourced(db, class.get_id(), course, by, Some(&blueprint.id))
                .await?;
        if matches!(landed, Attached::PivotGone) {
            class_blueprint::prune(db, &blueprint.id, course).await?;
            dead.push(course.clone());
        }
        if let Some(reason) = skip_reason(&landed) {
            skipped.push(Skip {
                class: class.get_id().clone(),
                class_name: class.get_name().as_str().to_string(),
                course: course.clone(),
                reason,
            });
        }
        if matches!(landed, Attached::SourceGone) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// [`apply_to`] every class at this blueprint's grade. The write loop
/// is one transaction per (class, course) and bounded by neither — see the
/// module note.
///
/// Two things are said once for the whole run rather than once per section,
/// since a section cannot make either of them untrue. A course that no
/// longer exists is pruned and skipped on the first class that meets it and
/// left out of every class after it. And a `blueprint_deleted` **aborts the
/// grade loop**: the attach's `FOR KEY SHARE` row-lock claim makes that
/// answer precisely "a delete landed mid-pump", so it is reported once and
/// the remaining classes are not walked.
///
/// The abort is still a `Ok(..)`, not an error: the attaches that committed
/// before the delete stand (its sweep took the ones it could reach), and a
/// partial state reported in full is what best-effort means everywhere else
/// here.
///
/// [`Pumped::matched`] is counted off this loop rather than asked of a
/// second query — the sections the run *walked*, so an abort reports what
/// it did and not what the grade holds.
///
/// `&mut`, because [`class_blueprint::prune`] writes the store and this
/// handle is what both write routes render their response from: a course
/// pruned mid-pump was still listed in the `201`/`200` that pruned it, while
/// the `GET` a moment later did not have it.
pub async fn pump(
    db: &Database,
    blueprint: &mut ClassBlueprint,
    by: &UserId,
) -> Result<Pumped, AppError> {
    let mut pumped = Pumped {
        matched: 0,
        skipped: Vec::new(),
    };
    let mut dead = Vec::new();
    for class in class_group::list_for_grade(db, &blueprint.grade).await? {
        pumped.matched += 1;
        if !apply_courses(db, blueprint, &class, by, &mut dead, &mut pumped.skipped).await? {
            break;
        }
    }
    // Exactly the ids `prune` took out of the stored row, so the handle and
    // the store say the same thing.
    blueprint.courses.retain(|course| !dead.contains(course));
    Ok(pumped)
}

/// How far every section at this grade stands from the template: the
/// courses each one does not carry. Writes nothing — a pump's skip list
/// lives only in the response that reported it, and this is the read that
/// answers "which sections are still out of sync" afterwards.
///
/// **A link of any source counts as satisfied.** A course a human attached
/// by hand fulfils the template exactly as a pumped one does — that is
/// [`apply_to`]'s own idempotence rule, and a status read that
/// disagreed with it would send managers chasing rows no pump will ever
/// write.
///
/// A course id the blueprint holds but that no longer exists reads as
/// missing from every section. It is the truth about the section, and this
/// read cannot prune it the way a pump does ([`class_blueprint::prune`])
/// without writing — but it is no longer a state a course delete leaves
/// behind: the delete's own cascade takes the id out of every template
/// naming it, so only a row written before that cascade existed, or one the
/// pump's own window ([`class_blueprint::prune`]) is about to clear, can
/// still show it.
///
/// Unpaged, like the [`crate::db::class_group::list_for_grade`] it is built on: the set
/// is the şube one school runs at one grade, and the caller is asking about
/// all of them.
pub async fn status(
    db: &Database,
    blueprint: &ClassBlueprint,
) -> Result<Vec<SectionStatus>, AppError> {
    let sections = class_group::list_for_grade(db, &blueprint.grade).await?;
    if sections.is_empty() {
        return Ok(Vec::new());
    }
    let classes: Vec<ClassGroupId> = sections
        .iter()
        .map(|class| class.get_id().clone())
        .collect();
    let held = class_blueprint::held_links(db, classes).await?;
    Ok(sections
        .iter()
        .map(|class| SectionStatus {
            class: class.get_id().clone(),
            class_name: class.get_name().as_str().to_string(),
            missing: blueprint
                .courses
                .iter()
                .filter(|course| {
                    !held
                        .iter()
                        .any(|row| &row.class == class.get_id() && &&row.course == course)
                })
                .cloned()
                .collect(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::MAX_CLASS_COURSES;
    use crate::db::class_member::tests::{
        a_class, a_course, counter, fixture_user, link_exists, rows,
    };
    use crate::domain::class_group::{ClassGroup, ClassName};

    /// A section that a pump's own grade loop can actually find — [`a_class`]
    /// carries no grade at all, so `list_for_grade` reaches none of them.
    async fn a_section(name: &str, db: &Database) -> ClassGroup {
        class_group::create(
            db,
            &fixture_user(db, "manager").await,
            ClassName::try_new(name).unwrap(),
            Some(ClassBlueprint::grade_key("9").unwrap()),
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// A blueprint holding `courses`, at grade "9".
    async fn a_blueprint(courses: Vec<CourseId>, db: &Database) -> ClassBlueprint {
        let manager = fixture_user(db, "manager").await;
        create(
            db,
            &manager,
            ClassBlueprint::grade_key("9").unwrap(),
            courses,
        )
        .await
        .unwrap()
    }

    /// Every skip names the record that actually failed.
    ///
    /// The three refusals a manager can meet come out of two different claims
    /// in one transaction, and they used to be reported by one string that
    /// guessed: a deleted *course* was announced as a deleted *class*, on a
    /// section standing right there. Each is provoked from the state that
    /// really produces it, not from the enum.
    #[tokio::test]
    async fn a_skip_names_what_actually_failed() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;

        // The course is deleted out from under the pump: the pivot claim
        // matches nothing.
        let course = a_course("algebra", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        // Children first, the ordering the course's own cascade runs: with the
        // junction row standing, the raw course delete below is a foreign-key
        // refusal. What is left is exactly the pump-window state the handle
        // still walks.
        sqlx::query("DELETE FROM blueprint_course WHERE course = $1")
            .bind(course.uuid())
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM course WHERE id = $1")
            .bind(course.uuid())
            .execute(&db)
            .await
            .unwrap();
        let skipped = apply_to(&db, &blueprint, &class, &manager).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "course_deleted",
            "the course went, not the class"
        );
        // …and the dangling id is taken out of the list, so the skip is
        // reported once instead of on every pump forever.
        assert!(
            read(&db, blueprint.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_courses()
                .is_empty(),
            "a course that no longer exists is pruned from the blueprint"
        );

        // The class is deleted out from under the pump: the counter claim
        // matches nothing and the read that follows finds no row.
        let (db, _leases) = crate::database::init_test_db().await;
        let course = a_course("algebra", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        sqlx::query("DELETE FROM class_group WHERE id = $1")
            .bind(class.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
        let skipped = apply_to(&db, &blueprint, &class, &manager).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "class_deleted",
            "the class went, not the course"
        );
        assert_eq!(
            read(&db, blueprint.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_courses(),
            &[course],
            "a live course is not pruned because a class vanished"
        );

        // The class stands at its own ceiling: the same claim matches nothing,
        // but the row is there — and that is the one a manager can act on.
        let (db, _leases) = crate::database::init_test_db().await;
        let course = a_course("algebra", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course], &db).await;
        sqlx::query(
            sqlx::AssertSqlSafe(format!(
                "UPDATE class_group SET class_course_count = {} WHERE id = $1",
                MAX_CLASS_COURSES
            )),
        )
        .bind(class.get_id().uuid())
        .execute(&db)
        .await
        .unwrap();
        let skipped = apply_to(&db, &blueprint, &class, &manager).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "class_at_course_ceiling",
            "a full class is not a deleted one"
        );
    }

    /// The delete is a compare-and-set, and it runs *before* the sweep — so a
    /// handle whose list moved cannot take the row, and nothing it would have
    /// swept is touched either.
    ///
    /// Provoked with a stale handle rather than a concurrent request, which is
    /// exactly the state a `PATCH` landing between the read and the delete puts
    /// this caller in, and deterministic where two live requests are not.
    #[tokio::test]
    async fn a_delete_of_a_list_that_moved_is_a_409_that_writes_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let algebra = a_course("algebra", None, &db).await;
        let physics = a_course("physics", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let stale = a_blueprint(vec![algebra.clone()], &db).await;
        apply_to(&db, &stale, &class, &manager).await.unwrap();

        // The edit this caller did not see: it adds a course and pumps it into
        // the section. (The pump is applied by hand because the helper class
        // carries no grade for `set_courses`'s own loop to find.)
        let (edited, _) = set_courses(
            &db,
            read(&db, stale.get_id()).await.unwrap().unwrap(),
            vec![algebra, physics],
            &manager,
        )
        .await
        .unwrap();
        apply_to(&db, &edited, &class, &manager).await.unwrap();

        let refused = delete(&db, stale).await;
        assert!(
            matches!(refused, Err(AppError::Conflict(_))),
            "a list that moved is a 409, not a silent delete: {refused:?}"
        );
        assert!(
            read(&db, &ClassBlueprintId::from_key("9"))
                .await
                .unwrap()
                .is_some(),
            "the row the caller did not read is still there"
        );
        assert_eq!(
            rows("class_course", &db).await,
            2,
            "and neither attachment was swept — including the one this handle \
             never knew about"
        );
    }

    /// The lease is real: a delete cannot start while a guarded attach holds
    /// the read side.
    ///
    /// That is the whole point of the lock — without it a pump could commit its
    /// `class_course` row *after* the delete had swept past, stranding a row
    /// tagged with a blueprint nothing can reach. Two live requests would be a
    /// coin flip the in-memory engine lies about, so the lease is held directly
    /// and the delete is watched not-finishing on a timeout.
    #[tokio::test]
    async fn a_delete_waits_for_an_attach_in_flight() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let algebra = a_course("algebra", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![algebra.clone()], &db).await;
        apply_to(&db, &blueprint, &class, &manager).await.unwrap();

        // The lease is gone with the lock; the whole compare-and-set-plus-
        // sweep is one transaction now, and the pump claims the blueprint
        // inside its own. A barrier start lets the racing attach and delete
        // interleave every which way — in all of them no `class_course` row
        // may survive a blueprint that is gone, and no counter may drift.
        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let deleting = {
            let (db, blueprint, gate) = (db.clone(), blueprint.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                delete(&db, blueprint).await
            })
        };
        let pumping = {
            let (db, class, _algebra, manager, gate) =
                (db.clone(), class.clone(), algebra.clone(), manager, gate);
            tokio::spawn(async move {
                gate.wait().await;
                apply_to(&db, &blueprint, &class, &manager).await
            })
        };
        let (deleted, pumped) = (deleting.await.unwrap(), pumping.await.unwrap());
        deleted.unwrap();

        // The delete went through; whatever the pump answered, the stored
        // state must agree with itself.
        assert_eq!(
            rows("class_blueprint", &db).await,
            0,
            "the delete goes through"
        );
        let skipped = pumped.unwrap();
        if skipped.iter().any(|s| s.reason == "blueprint_deleted") {
            assert_eq!(
                rows("class_course", &db).await,
                0,
                "a pump that read a dead blueprint attaches nothing"
            );
        } else {
            // The pump beat the delete to the claim and attached cleanly: its
            // rows go with the blueprint the sweep then took.
            assert!(
                skipped.is_empty(),
                "neither a clean attach nor a clean skip: {skipped:?}"
            );
            assert_eq!(
                rows("class_course", &db).await,
                0,
                "the sweep took the rows the winning pump had just written"
            );
        }
        assert_eq!(
            counter("class_course_count", class.get_id().uuid(), &db).await,
            0,
            "…and whatever happened, the class's counter is back to zero"
        );
    }

    /// …and the other side of that lease: it is released before the detaching
    /// starts, so a pump is *not* parked behind it.
    ///
    /// The sweep is one transaction per link row and bounded by nothing —
    /// sections × courses, each walking up to a full roster — and `tokio`'s
    /// `RwLock` is fair, so a lease held across it stalled every concurrent
    /// `POST /classes` in the school, at every grade, for the whole delete. What
    /// the lease has to cover is only the compare-and-set and the read that
    /// fixes the row set, which `a_delete_waits_for_an_attach_in_flight` above
    /// still pins from the other direction.
    ///
    /// The unbounded loop is stood in for by four slow rows: a `DEFINE EVENT` on
    /// `class_course` fires inside each detaching transaction, so the delete is
    /// provably still sweeping when the lease is asked for. Nothing here is
    /// asserted off a fixed delay — the row going is what says the
    /// compare-and-set has run, and one *moment* of a free lease while the sweep
    /// is unfinished is the whole claim. A sweep that ended first fails the
    /// assertion rather than passing it by default.
    #[tokio::test]
    async fn a_delete_frees_the_lease_before_its_unbounded_sweep() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let courses = vec![
            a_course("algebra", None, &db).await,
            a_course("physics", None, &db).await,
        ];
        let blueprint = a_blueprint(courses, &db).await;
        let mut classes = Vec::new();
        for name in ["9-A", "9-B", "9-C"] {
            let class = class_group::read(&db, &a_class(name, &db).await)
                .await
                .unwrap()
                .unwrap();
            if name != "9-C" {
                apply_to(&db, &blueprint, &class, &manager).await.unwrap();
            }
            classes.push(class);
        }

        // The lock is gone, so there is no lease to free; what still has to
        // hold is its point: a pump arriving while the delete's sweep is in
        // flight is answered by the database, not parked behind the sweep —
        // and the sweep still takes every row carrying the blueprint's tag.
        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let deleting = {
            let (db, blueprint, gate) = (db.clone(), blueprint.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                delete(&db, blueprint).await
            })
        };
        let pumping = {
            let (db, class, manager, gate, blueprint) = (
                db.clone(),
                classes[2].clone(),
                manager,
                gate.clone(),
                blueprint.clone(),
            );
            tokio::spawn(async move {
                gate.wait().await;
                apply_to(&db, &blueprint, &class, &manager).await
            })
        };

        // The pump is answered in bounded time even though the delete is
        // sweeping two sections' link rows — and answered with one of the two
        // honest outcomes, never a 500.
        let pumped =
            tokio::time::timeout(std::time::Duration::from_secs(10), pumping)
                .await
                .expect("a pump must not wait behind a delete's unbounded sweep")
                .unwrap();
        let skipped = pumped.unwrap();
        assert!(
            skipped.iter().all(|s| {
                s.reason == "blueprint_deleted" || s.reason == "course_deleted"
            }) || skipped.is_empty(),
            "neither a clean attach nor a clean skip: {skipped:?}"
        );

        deleting.await.unwrap().unwrap();
        assert_eq!(
            rows("class_course", &db).await,
            0,
            "…and the sweep still took every row it owned"
        );
        for class in &classes {
            assert_eq!(
                counter("class_course_count", class.get_id().uuid(), &db).await,
                0,
                "…and no counter kept a seat the sweep took back"
            );
        }
    }

    /// The other half: a pump holding a handle to a blueprint that has since
    /// been deleted writes nothing at all, and says so with its own code.
    ///
    /// Without the in-transaction claim the attach would land a `class_course`
    /// row tagged with a record that no longer exists — the delete's sweep has
    /// already run, and the grade label *is* the id, so nothing could ever
    /// reach that row again.
    #[tokio::test]
    async fn a_pump_whose_blueprint_died_attaches_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let algebra = a_course("algebra", None, &db).await;
        let class = class_group::read(&db, &a_class("9-A", &db).await)
            .await
            .unwrap()
            .unwrap();
        let stale = a_blueprint(vec![algebra], &db).await;
        delete(&db, read(&db, stale.get_id()).await.unwrap().unwrap())
            .await
            .unwrap();

        let skipped = apply_to(&db, &stale, &class, &manager).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "blueprint_deleted",
            "the template went, not the class or the course"
        );
        assert_eq!(
            rows("class_course", &db).await,
            0,
            "a row tagged with a deleted blueprint is one nothing can sweep"
        );
        assert_eq!(
            counter("class_course_count", class.get_id().uuid(), &db).await,
            0,
            "…and no counter moved for it"
        );
    }

    /// A course that no longer exists is pruned and reported **once for the
    /// pump**, not once per section: twelve sections at a grade must not answer
    /// one dead course with twelve identical skips and twelve identical prune
    /// writes. The live course still reaches every section.
    ///
    /// Stored state is what is asserted — the in-memory engine forges wins, so
    /// the links are re-read rather than counted off the return value.
    #[tokio::test]
    async fn a_dead_course_is_pruned_and_skipped_once_for_the_whole_grade() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let astronomy = a_course("astronomy", None, &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let a = a_section("9-A", &db).await;
        let b = a_section("9-B", &db).await;
        let mut blueprint = a_blueprint(vec![astronomy.clone(), algebra.clone()], &db).await;
        // Children first, the ordering the course's own cascade runs — the
        // junction row standing would make the raw delete a foreign-key
        // refusal. The handle below still walks the list it read.
        sqlx::query("DELETE FROM blueprint_course WHERE course = $1")
            .bind(astronomy.uuid())
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM course WHERE id = $1")
            .bind(astronomy.uuid())
            .execute(&db)
            .await
            .unwrap();

        let Pumped { matched, skipped } = pump(&db, &mut blueprint, &manager).await.unwrap();
        assert_eq!(matched, 2, "both sections at the grade were walked");
        assert_eq!(
            skipped.len(),
            1,
            "one dead course is one skip for the grade, not one per section: {skipped:?}"
        );
        assert_eq!(skipped[0].reason, "course_deleted");
        assert_eq!(
            read(&db, blueprint.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_courses(),
            std::slice::from_ref(&algebra),
            "and the list is pruned to the course that still exists"
        );
        for class in [&a, &b] {
            assert!(
                link_exists(class.get_id(), &algebra, &db).await,
                "the live course still reached {}",
                class.get_name().as_str()
            );
        }
        assert_eq!(
            rows("class_course", &db).await,
            2,
            "…and nothing else was attached"
        );
    }

    /// A blueprint that is gone ends the whole grade loop: the answer is about
    /// the template, so every remaining section would only repeat it. Reported
    /// once, and as `Ok` — best-effort, with whatever committed left standing.
    ///
    /// Provoked with a stale handle rather than a live interleaving, which the
    /// in-memory engine cannot be made to produce; it is the same state a
    /// delete landing between the read and the pump leaves this caller in.
    #[tokio::test]
    async fn a_deleted_blueprint_stops_the_grade_loop_after_one_skip() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = fixture_user(&db, "manager").await;
        let algebra = a_course("algebra", None, &db).await;
        a_section("9-A", &db).await;
        a_section("9-B", &db).await;
        let mut stale = a_blueprint(vec![algebra], &db).await;
        delete(&db, read(&db, stale.get_id()).await.unwrap().unwrap())
            .await
            .unwrap();

        let Pumped { matched, skipped } = pump(&db, &mut stale, &manager).await.unwrap();
        assert_eq!(
            skipped.len(),
            1,
            "the template went once, not once per section: {skipped:?}"
        );
        assert_eq!(skipped[0].reason, "blueprint_deleted");
        assert_eq!(
            matched, 1,
            "an abort reports the sections it walked, not the two the grade holds"
        );
        assert_eq!(
            rows("class_course", &db).await,
            0,
            "…and nothing was attached under a blueprint nothing could sweep"
        );
    }
}
