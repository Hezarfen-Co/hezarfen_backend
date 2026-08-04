//! A grade's course template: the courses every class section (şube) at one
//! grade carries.
//!
//! A school runs many şube at grade "9" and stocks each of them by hand, one
//! attach at a time. A blueprint is that list said once. It is a layer *above*
//! [`crate::domain::class_pump`], never a replacement for it: applying a
//! blueprint calls the same [`ClassCourse::attach`] a manager's own call does,
//! so the rows it lands are ordinary `class_course` links and ordinary
//! `enrollment` rows, and an elective (seçmeli) placed by hand next to them is
//! still an individual enrollment nothing here can see.
//!
//! Two rules shape everything below.
//!
//! **An edit retro-pumps.** Changing the list reaches every class already at
//! that grade, not just the ones made afterwards. That is an unbounded write
//! loop — one transaction per (class, course), and no ceiling bounds the number
//! of classes — chosen deliberately over a template that only new classes see.
//!
//! **A pump is best-effort.** Each (class, course) pair is one all-or-nothing
//! transaction of the existing pump, and a pair that would breach a limit is
//! *skipped and reported* rather than aborting the other classes' share. So a
//! blueprint edit can leave a partial state — which is the point: one full
//! course must not stop the other eleven sections from being stocked. Every
//! skip is returned, naming the class, the course and the reason.
//!
//! Removal is the mirror, and it is where the provenance tag earns its keep: a
//! `class_course` row this blueprint wrote carries `source`, a row a human
//! attached carries no such key at all, and dropping a course from the
//! blueprint sweeps only the former. The sweep is the pump's own
//! [`crate::domain::class_pump::detach`], so a class losing a course still
//! repairs before it deletes — and it runs one transaction per pair too, for
//! the same reason the pump does.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::RwLock;

use crate::constant::{CLASS_BLUEPRINT_TABLE, CLASS_COURSE_TABLE, MAX_CLASS_COURSES};
use crate::database::Database;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId};
use crate::domain::class_pump::{Attached, Axis, detach};
use crate::domain::course::CourseId;
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes a blueprint's delete against the attaches made on its behalf.
///
/// [`ClassBlueprint::delete`] removes the row and then sweeps by the provenance
/// tag, and every guarded attach reads that same row *inside* its own
/// transaction ([`crate::domain::class_pump::attach`]'s `source` claim). Those
/// two are a cross-record read-then-write racing a write to the record read,
/// which `BEGIN`/`COMMIT` does not serialize (SurrealDB write skew): a pump can
/// see the blueprint alive, have the delete commit and sweep past it, and only
/// then commit its own `class_course` row — tagged with a record that no longer
/// exists and that no sweep can ever reach again, since the grade label *is*
/// the id. Delete-first and the in-transaction claim narrow that window; this
/// closes it.
///
/// The delete holds the **write** lease across its compare-and-set and its
/// sweep, so the sweep sees every attach that committed before it and no attach
/// commits after it. Each guarded attach holds the **read** lease for the span
/// of its own transaction — taken per course in [`ClassBlueprint::apply_to`],
/// never around a whole pump, so a delete waits behind one attach rather than
/// an unbounded loop. `tokio`'s `RwLock` is fair, so a steady stream of pump
/// leases cannot starve that waiting delete.
///
/// In-process is deployment-wide here: single-replica by decision, with
/// stop-the-world upgrades — the same argument
/// [`crate::domain::settings::SETTINGS_LOCK`] makes. No other lock is taken
/// under it, so it has no ordering rule to break.
pub(crate) static BLUEPRINT_LOCK: RwLock<()> = RwLock::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassBlueprintId(RecordId);

impl ClassBlueprintId {
    /// The record one grade label always maps to. The label is the key, so
    /// "one blueprint per grade" holds by construction rather than by a
    /// find-then-insert that two managers can race — the shape
    /// [`crate::domain::menu::MenuId::for_slot`] uses.
    pub fn for_grade(grade: &ClassGrade) -> Self {
        Self(RecordId::new(CLASS_BLUEPRINT_TABLE, grade.as_str()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(CLASS_BLUEPRINT_TABLE, key))
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

/// One grade's template. `grade` is stored beside the key it *is*, so a read
/// never has to parse a record id back into a domain value.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassBlueprint {
    id: ClassBlueprintId,
    grade: ClassGrade,
    courses: Vec<CourseId>,
    creator: UserId,
}

/// One (class, course) pair a pump refused, and why. Precise enough to act on:
/// the class is named as well as identified, because a manager reading "12
/// skipped" needs to know *which* section is short a course.
#[derive(Debug, Clone)]
pub struct Skip {
    pub class: ClassGroupId,
    pub class_name: String,
    pub course: CourseId,
    pub reason: &'static str,
}

/// What a pump did: how many sections it reached, and the pairs it refused.
///
/// `matched` exists because an empty `skipped` is not success on its own. A
/// grade label is free text ([`ClassGrade`]) and [`ClassGroup::list_for_grade`]
/// matches it exactly, so a blueprint keyed `"9 "` reaches none of the sections
/// keyed `"9"` — and with nothing to skip it answers exactly like a pump that
/// stocked every one of them. The count is the only thing that tells those
/// apart.
///
/// A struct rather than a pair with [`ClassBlueprint::set_courses`]'s: three
/// unlabelled fields off one call is a shape a caller has to remember.
#[derive(Debug)]
pub struct Pumped {
    /// The sections this pump **reached** — not the ones the grade holds. The
    /// two differ when the run ended early (`blueprint_deleted`), and what
    /// happened is the actionable one.
    pub matched: i64,
    pub skipped: Vec<Skip>,
}

/// One section's distance from its grade's template: the courses it does not
/// carry. Named as well as identified, like [`Skip`], and for the same reason.
#[derive(Debug, Clone)]
pub struct SectionStatus {
    pub class: ClassGroupId,
    pub class_name: String,
    pub missing: Vec<CourseId>,
}

/// One `class_course` row, projected down to the pair that answers "does this
/// section carry that course". `source` is deliberately not read: a link of any
/// provenance satisfies the template.
#[derive(SurrealValue)]
struct Held {
    class: ClassGroupId,
    course: CourseId,
}

/// Why this attach did not land, or `None` when it did — the shared vocabulary
/// of [`Attached::refusal_code`], which is where the codes and the set they
/// close over are documented.
///
/// One divergence, and it lives here because only a pump has it: a duplicate is
/// not a skip. The course is already on the class, which is exactly what the
/// blueprint asks for, so re-running a pump must report nothing — while a *hand*
/// attach's duplicate is a genuine refusal (`duplicate`), because that call
/// asked for the row and did not get it.
///
/// Always the course axis: a pump attaches courses to sections, so the two
/// ceiling codes are read on that side ([`Attached::refusal_code`] takes the
/// axis because they differ per axis).
fn skip_reason(landed: &Attached<ClassCourse>) -> Option<&'static str> {
    match landed {
        Attached::Duplicate => None,
        other => other.refusal_code(&Axis::Course),
    }
}

impl ClassBlueprint {
    pub fn get_id(&self) -> &ClassBlueprintId {
        &self.id
    }

    pub fn get_grade(&self) -> &ClassGrade {
        &self.grade
    }

    pub fn get_courses(&self) -> &[CourseId] {
        &self.courses
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    /// The grade a blueprint may be keyed on. Non-empty, because the label is
    /// the record id and there is no blueprint for "no grade"; and free of the
    /// characters that would make that id unaddressable as a URL path segment,
    /// the second gate [`crate::domain::menu::MenuSlot`] carries for the same
    /// reason. Grades were never validated for this, so a *class* may already
    /// carry a label refused here — it simply cannot have a blueprint until it
    /// is renamed, which is a 400 the caller can read rather than a route
    /// nobody can reach.
    pub fn grade_key(value: &str) -> Result<ClassGrade, AppError> {
        if value.is_empty() {
            return Err(AppError::Validation(ValidationError::Empty("grade")));
        }
        if value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%'))
        {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "grade",
                reason: "must not contain / \\ ? # or %",
            }));
        }
        Ok(ClassGrade::try_new(value)?)
    }

    /// The course list a blueprint may hold: deduplicated, and no longer than
    /// one class may carry — a blueprint above that ceiling would guarantee a
    /// skip on every class it ever reached.
    fn course_list(courses: Vec<CourseId>) -> Result<Vec<CourseId>, AppError> {
        let mut list: Vec<CourseId> = Vec::new();
        for course in courses {
            if !list.contains(&course) {
                list.push(course);
            }
        }
        if list.len() as i64 > MAX_CLASS_COURSES {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "course_ids",
                reason: "a blueprint cannot hold more courses than a class may carry \
                         (max_class_courses)",
            }));
        }
        Ok(list)
    }

    /// Write the blueprint. A second one for the same grade is a 409 the store
    /// itself decides — the grade is the record key, so the duplicate is seen
    /// rather than raced.
    pub async fn create(
        creator: &UserId,
        grade: ClassGrade,
        courses: Vec<CourseId>,
        db: &Database,
    ) -> Result<ClassBlueprint, AppError> {
        let blueprint = ClassBlueprint {
            id: ClassBlueprintId::for_grade(&grade),
            grade,
            courses: Self::course_list(courses)?,
            creator: creator.clone(),
        };
        match db
            .create::<Option<ClassBlueprint>>(blueprint.id.record())
            .content(blueprint)
            .await
        {
            Ok(Some(created)) => Ok(created),
            Ok(None) => Err(AppError::Internal("failed to create blueprint".into())),
            Err(error) if error.is_already_exists() => Err(AppError::Conflict(
                "a blueprint already exists for that grade",
            )),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn read(
        id: &ClassBlueprintId,
        db: &Database,
    ) -> Result<Option<ClassBlueprint>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every blueprint, by grade label — the id *is* the label, so this is the
    /// only ordering that means anything here.
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
        PagedList::new(CLASS_BLUEPRINT_TABLE, "ORDER BY id ASC")
            .run(limit, offset, db)
            .await
    }

    /// Replace the course list, then reconcile every class at this grade with
    /// it: the courses this blueprint no longer holds are detached from the
    /// classes *it* attached them to, and the ones it holds are pumped into
    /// every class that fits.
    ///
    /// The write is a compare-and-set on the list this caller read, so two
    /// managers editing the same grade cannot have one's list silently pump the
    /// other's diff — the loser is a 409 and re-reads.
    ///
    /// Removals run first: a course leaving frees a place under the per-class
    /// course ceiling that the same edit's additions can then use.
    ///
    /// Takes no [`BLUEPRINT_LOCK`] lease, deliberately: holding the write lease
    /// across its own pump would deadlock on the read lease that pump takes,
    /// and a lease would close nothing anyway — the pump's claim asks whether
    /// the blueprint *exists*, not what it holds, so an edit racing a pump is
    /// only ever the retro-pump this feature is built on. Rows a racing edit
    /// leaves behind stay reachable, because [`Self::delete`] sweeps the whole
    /// tag rather than a list.
    pub async fn set_courses(
        self,
        courses: Vec<CourseId>,
        by: &UserId,
        db: &Database,
    ) -> Result<(ClassBlueprint, Pumped), AppError> {
        let wanted = Self::course_list(courses)?;
        let dropped: Vec<CourseId> = self
            .courses
            .iter()
            .filter(|course| !wanted.contains(course))
            .cloned()
            .collect();
        let mut result = db
            .query("UPDATE $id SET courses = $wanted WHERE courses = $held RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("wanted", wanted))
            .bind(("held", self.courses.clone()))
            .await?
            .check()?;
        let Some(saved) = result.take::<Vec<ClassBlueprint>>(0)?.into_iter().next() else {
            // The conditional write matched nothing: the row is gone, or its
            // list moved since this caller read it. Only this path pays for the
            // read that tells those apart.
            return match Self::read(&self.id, db).await? {
                Some(_) => Err(AppError::Conflict(
                    "this blueprint changed since you read it — re-read and retry",
                )),
                None => Err(AppError::NotFound),
            };
        };
        saved.drop_courses(Some(&dropped), db).await?;
        let pumped = saved.pump(by, db).await?;
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
    /// record id, so only a blueprint recreated at that grade could even name
    /// them. Never revert to sweep-first.
    ///
    /// Deleting first plus the pump's own in-transaction claim on this row
    /// ([`Attached::SourceGone`]) **narrows** that window; it does not close
    /// it. The claim is a read of `class_blueprint` in a transaction that
    /// writes `class_course`, racing this delete's write to
    /// `class_blueprint` — a cross-record pair `BEGIN`/`COMMIT` does not
    /// serialize, so a pump that read the row alive can still commit its link
    /// after the sweep has run. [`BLUEPRINT_LOCK`] is what closes it: the write
    /// lease below spans the compare-and-set and the sweep, the read lease in
    /// [`Self::apply_to`] spans each attach, and single-replica deployment
    /// makes an in-process lock the whole answer.
    ///
    /// What is left is a **process crash** between the delete and the sweep — a
    /// lock does not survive the process. That leaves inert `class_course` rows
    /// tagged with a blueprint that is gone; they are still detachable one at a
    /// time at `DELETE /classes/{id}/courses/{course}`, and every counter stays
    /// exact because each detach is its own transaction.
    ///
    /// The delete is a compare-and-set on the list this caller read, like
    /// [`Self::set_courses`]: an edit landing in between is a `409` rather than
    /// a silent detach of somebody else's additions. Known hole, accepted: the
    /// grade label *is* the record id and the comparison is by **content**, so
    /// a blueprint deleted and recreated at the same grade with the same list
    /// satisfies `WHERE courses = $held` and this call deletes the *new* row.
    /// Content-equal is intent-equal — the end state is the one the caller
    /// asked for — and telling the two apart needs a revision column on the
    /// row, which nothing else here would use.
    pub async fn delete(self, db: &Database) -> Result<(), AppError> {
        // Held through the sweep: it must see every attach that committed
        // before it, and no attach may commit after it.
        let _lease = BLUEPRINT_LOCK.write().await;
        let mut result = db
            .query("DELETE $id WHERE courses = $held RETURN BEFORE")
            .bind(("id", self.id.record()))
            .bind(("held", self.courses.clone()))
            .await?
            .check()?;
        if result.take::<Vec<ClassBlueprint>>(0)?.is_empty() {
            // Matched nothing: the row is gone, or its list moved since this
            // caller read it. Only this path pays for the read that tells them
            // apart.
            return match Self::read(&self.id, db).await? {
                Some(_) => Err(AppError::Conflict(
                    "this blueprint changed since you read it — re-read and retry",
                )),
                None => Err(AppError::NotFound),
            };
        }
        self.drop_courses(None, db).await
    }

    /// Attach every course in this blueprint to `class`, skipping — never
    /// aborting on — the ones that do not fit. A course already on the class is
    /// a no-op, whoever attached it: this is what makes a pump repeatable, and
    /// what stops it re-tagging a hand-attached course as its own.
    ///
    /// This is the *only* place a sourced attach is made — [`Self::pump`], the
    /// create-time and per-class pumps in [`crate::web::classes`] all come
    /// through here — so it is where each one takes [`BLUEPRINT_LOCK`] for
    /// reading, one course at a time. A delete landing mid-pump is then clean
    /// by construction: the attaches that already committed are found by its
    /// tag sweep, and the ones that have not yet started meet the deleted row
    /// at their own in-transaction claim and answer `blueprint_deleted`. Per
    /// course rather than per pump, because the pump's loop is unbounded and a
    /// delete may not wait behind all of it.
    ///
    /// One class, so `blueprint_deleted` is an ordinary skip here: the courses
    /// after it are not tried, because the template they would ask for is the
    /// one that just went.
    pub async fn apply_to(
        &self,
        class: &ClassGroup,
        by: &UserId,
        db: &Database,
    ) -> Result<Vec<Skip>, AppError> {
        let mut skipped = Vec::new();
        self.apply_courses(class, by, &mut Vec::new(), &mut skipped, db)
            .await?;
        Ok(skipped)
    }

    /// One class's share of a pump, and the body both callers above share.
    ///
    /// `dead` is the courses this run already found deleted: they are not
    /// attempted again, so neither the skip nor [`Self::prune`]'s write repeats
    /// on the next section. A course newly found gone joins it.
    ///
    /// Answers `false` when the blueprint itself is gone — a caller looping
    /// over classes must stop, because every remaining one would answer exactly
    /// the same.
    async fn apply_courses(
        &self,
        class: &ClassGroup,
        by: &UserId,
        dead: &mut Vec<CourseId>,
        skipped: &mut Vec<Skip>,
        db: &Database,
    ) -> Result<bool, AppError> {
        for course in &self.courses {
            if dead.contains(course) {
                continue;
            }
            let landed = {
                let _lease = BLUEPRINT_LOCK.read().await;
                ClassCourse::attach_sourced(class.get_id(), course, by, Some(&self.id), db).await?
            };
            if matches!(landed, Attached::PivotGone) {
                self.prune(course, db).await?;
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

    /// [`Self::apply_to`] every class at this blueprint's grade. The write loop
    /// is one transaction per (class, course) and bounded by neither — see the
    /// module note.
    ///
    /// Two things are said once for the whole run rather than once per section,
    /// since a section cannot make either of them untrue. A course that no
    /// longer exists is pruned and skipped on the first class that meets it and
    /// left out of every class after it. And a `blueprint_deleted` **aborts the
    /// grade loop**: under [`BLUEPRINT_LOCK`] that answer is precisely "a
    /// delete landed mid-pump", so it is reported once and the remaining
    /// classes are not walked.
    ///
    /// The abort is still a `Ok(..)`, not an error: the attaches that committed
    /// before the delete stand (its sweep took the ones it could reach), and a
    /// partial state reported in full is what best-effort means everywhere else
    /// here.
    ///
    /// [`Pumped::matched`] is counted off this loop rather than asked of a
    /// second query — the sections the run *walked*, so an abort reports what
    /// it did and not what the grade holds.
    pub async fn pump(&self, by: &UserId, db: &Database) -> Result<Pumped, AppError> {
        let mut pumped = Pumped {
            matched: 0,
            skipped: Vec::new(),
        };
        let mut dead = Vec::new();
        for class in ClassGroup::list_for_grade(&self.grade, db).await? {
            pumped.matched += 1;
            if !self
                .apply_courses(&class, by, &mut dead, &mut pumped.skipped, db)
                .await?
            {
                break;
            }
        }
        Ok(pumped)
    }

    /// How far every section at this grade stands from the template: the
    /// courses each one does not carry. Writes nothing — a pump's skip list
    /// lives only in the response that reported it, and this is the read that
    /// answers "which sections are still out of sync" afterwards.
    ///
    /// **A link of any source counts as satisfied.** A course a human attached
    /// by hand fulfils the template exactly as a pumped one does — that is
    /// [`Self::apply_to`]'s own idempotence rule, and a status read that
    /// disagreed with it would send managers chasing rows no pump will ever
    /// write.
    ///
    /// A course id the blueprint holds but that no longer exists reads as
    /// missing from every section. It is the truth about the section, and this
    /// read cannot prune it the way a pump does ([`Self::prune`]) without
    /// writing; the next pump — `PATCH`ing the same list back — drops the id
    /// and this noise with it.
    ///
    /// Unpaged, like the [`ClassGroup::list_for_grade`] it is built on: the set
    /// is the şube one school runs at one grade, and the caller is asking about
    /// all of them.
    pub async fn status(&self, db: &Database) -> Result<Vec<SectionStatus>, AppError> {
        let sections = ClassGroup::list_for_grade(&self.grade, db).await?;
        if sections.is_empty() {
            return Ok(Vec::new());
        }
        let classes: Vec<RecordId> = sections
            .iter()
            .map(|class| class.get_id().record())
            .collect();
        let mut result = db
            .query(format!(
                "SELECT class, course FROM {CLASS_COURSE_TABLE} WHERE class IN $classes"
            ))
            .bind(("classes", classes))
            .await?
            .check()?;
        let held = result.take::<Vec<Held>>(0)?;
        Ok(sections
            .iter()
            .map(|class| SectionStatus {
                class: class.get_id().clone(),
                class_name: class.get_name().as_str().to_string(),
                missing: self
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

    /// Drop a course that no longer exists out of this blueprint's list.
    ///
    /// [`crate::domain::course::Course::delete`] takes the `class_course` links
    /// a course had, but nothing it can reach names the blueprints holding its
    /// id — so a deleted course stays in the list and every future pump refuses
    /// it again, on every class, forever. A skip a manager cannot act on is
    /// noise, so the pump that *finds* the dangling id also removes it: once
    /// for the run that found it (later classes in the same [`Self::pump`] skip
    /// it by its dead-course set rather than re-attempting and re-pruning it),
    /// and never again afterwards, because the list no longer holds it.
    ///
    /// Not a compare-and-set, unlike [`Self::set_courses`]: "a course that does
    /// not exist is not in this list" holds for every version of the list, so
    /// there is nothing a concurrent edit could make this write wrong about,
    /// and re-running it changes nothing. No sweep follows it either — the
    /// attachments it would sweep are exactly the ones the course's own delete
    /// already took.
    ///
    /// No [`BLUEPRINT_LOCK`] lease either: an `UPDATE` of a record that is gone
    /// writes nothing, so this cannot resurrect a blueprint a delete took while
    /// the pump around it was running.
    async fn prune(&self, course: &CourseId, db: &Database) -> Result<(), AppError> {
        db.query("UPDATE $id SET courses = array::complement(courses ?? [], [$course])")
            .bind(("id", self.id.record()))
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Detach this blueprint's attachments — the ones for `courses`, or *every*
    /// row carrying its tag when `None` — and sweep the enrollments they
    /// pumped.
    ///
    /// `None` is the delete's whole point: it asks the tag rather than a list,
    /// so a course a racing edit added and pumped after this caller read the
    /// row is swept too, which no snapshot of the list could ever name.
    ///
    /// `source` is the whole filter, so a row without the key — a hand attach —
    /// is never matched, and a class that acquired the same course by hand
    /// keeps it. The sweep underneath is the pump's own, so a student a second
    /// class still claims is re-tagged rather than unenrolled.
    ///
    /// One transaction per *link row*, which is the module note's "one
    /// transaction per (class, course)" and the mirror of the pump's
    /// best-effort rule. One statement per course would be shorter, but it puts
    /// every section at the grade into a single unbounded transaction: its write
    /// loop is one enrollment sweep per (class, member) with no ceiling over the
    /// class count, and a failure anywhere in it rolls back the whole grade —
    /// so one contended section keeps the other eleven attached to a course the
    /// template no longer holds. Per row, a failure leaves the pairs already
    /// detached detached, each with its counter released and its enrollments
    /// swept, and the rest exactly as they were: a partial removal, which is the
    /// same partial state a pump is allowed to leave.
    ///
    /// The rows are read first rather than derived from the classes at this
    /// grade: a class whose grade was edited after the pump still carries this
    /// blueprint's attachments, and only the `source` tag can find it.
    async fn drop_courses(
        &self,
        courses: Option<&[CourseId]>,
        db: &Database,
    ) -> Result<(), AppError> {
        let named: Vec<RecordId> = courses
            .unwrap_or_default()
            .iter()
            .map(CourseId::record)
            .collect();
        if courses.is_some() && named.is_empty() {
            return Ok(());
        }
        let only = if courses.is_some() {
            "AND course IN $courses"
        } else {
            ""
        };
        let mut found = db
            .query(format!(
                "SELECT VALUE id FROM {CLASS_COURSE_TABLE} WHERE source = $blueprint {only}"
            ))
            .bind(("blueprint", self.id.record()))
            .bind(("courses", named))
            .await?
            .check()?;
        for link in found.take::<Vec<RecordId>>(0)? {
            detach(
                "$link",
                Axis::Course,
                &[("link".into(), link.into_value())],
                db,
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::CLASS_COURSE_COUNT_FIELD;
    use crate::domain::class_course::ClassCourseId;
    use crate::domain::class_group::ClassName;
    use crate::domain::class_member::tests::{a_class, a_course, counter, exists, rows};

    /// A section that a pump's own grade loop can actually find — [`a_class`]
    /// carries no grade at all, so `list_for_grade` reaches none of them.
    async fn a_section(name: &str, db: &Database) -> ClassGroup {
        ClassGroup::create(
            &UserId::from_key("manager"),
            ClassName::try_new(name).unwrap(),
            Some(ClassBlueprint::grade_key("9").unwrap()),
            None,
            None,
            db,
        )
        .await
        .unwrap()
    }

    /// A blueprint holding `courses`, at grade "9".
    async fn a_blueprint(courses: Vec<CourseId>, db: &Database) -> ClassBlueprint {
        ClassBlueprint::create(
            &UserId::from_key("manager"),
            ClassBlueprint::grade_key("9").unwrap(),
            courses,
            db,
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
        let manager = UserId::from_key("manager");

        // The course is deleted out from under the pump: the pivot claim
        // matches nothing.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        db.query("DELETE $c")
            .bind(("c", course.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "course_deleted",
            "the course went, not the class"
        );
        // …and the dangling id is taken out of the list, so the skip is
        // reported once instead of on every pump forever.
        assert!(
            ClassBlueprint::read(blueprint.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_courses()
                .is_empty(),
            "a course that no longer exists is pruned from the blueprint"
        );

        // The class is deleted out from under the pump: the counter claim
        // matches nothing and the read that follows finds no row.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        db.query("DELETE $c")
            .bind(("c", class.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "class_deleted",
            "the class went, not the course"
        );
        assert_eq!(
            ClassBlueprint::read(blueprint.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_courses(),
            &[course],
            "a live course is not pruned because a class vanished"
        );

        // The class stands at its own ceiling: the same claim matches nothing,
        // but the row is there — and that is the one a manager can act on.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course], &db).await;
        db.query(format!(
            "UPDATE $c SET {} = $cap",
            crate::constant::CLASS_COURSE_COUNT_FIELD
        ))
        .bind(("c", class.get_id().record()))
        .bind(("cap", MAX_CLASS_COURSES))
        .await
        .unwrap()
        .check()
        .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let algebra = a_course("algebra", None, &db).await;
        let physics = a_course("physics", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let stale = a_blueprint(vec![algebra.clone()], &db).await;
        stale.apply_to(&class, &manager, &db).await.unwrap();

        // The edit this caller did not see: it adds a course and pumps it into
        // the section. (The pump is applied by hand because the helper class
        // carries no grade for `set_courses`'s own loop to find.)
        let (edited, _) = ClassBlueprint::read(stale.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .set_courses(vec![algebra, physics], &manager, &db)
            .await
            .unwrap();
        edited.apply_to(&class, &manager, &db).await.unwrap();

        let refused = stale.delete(&db).await;
        assert!(
            matches!(refused, Err(AppError::Conflict(_))),
            "a list that moved is a 409, not a silent delete: {refused:?}"
        );
        assert!(
            ClassBlueprint::read(&ClassBlueprintId::from_key("9"), &db)
                .await
                .unwrap()
                .is_some(),
            "the row the caller did not read is still there"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let algebra = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![algebra], &db).await;
        blueprint.apply_to(&class, &manager, &db).await.unwrap();

        // Stand in for an attach mid-transaction, which is exactly the lease
        // `apply_to` holds around one course.
        let attaching = BLUEPRINT_LOCK.read().await;
        let mut deleting = {
            let db = db.clone();
            let blueprint = blueprint.clone();
            tokio::spawn(async move { blueprint.delete(&db).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut deleting)
                .await
                .is_err(),
            "the delete must not run while an attach is in flight"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_blueprint", &db).await,
            1,
            "…and it must not have swept anything either"
        );

        drop(attaching);
        deleting.await.unwrap().unwrap();
        assert_eq!(
            rows("SELECT VALUE id FROM class_blueprint", &db).await,
            0,
            "once the attach is done the delete goes through"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
            0,
            "…taking every row carrying its tag with it"
        );
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let algebra = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let stale = a_blueprint(vec![algebra], &db).await;
        ClassBlueprint::read(stale.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .delete(&db)
            .await
            .unwrap();

        let skipped = stale.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "blueprint_deleted",
            "the template went, not the class or the course"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
            0,
            "a row tagged with a deleted blueprint is one nothing can sweep"
        );
        assert_eq!(
            counter(CLASS_COURSE_COUNT_FIELD, class.get_id().record(), &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let astronomy = a_course("astronomy", None, &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let a = a_section("9-A", &db).await;
        let b = a_section("9-B", &db).await;
        let blueprint = a_blueprint(vec![astronomy.clone(), algebra.clone()], &db).await;
        db.query("DELETE $c")
            .bind(("c", astronomy.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let Pumped { matched, skipped } = blueprint.pump(&manager, &db).await.unwrap();
        assert_eq!(matched, 2, "both sections at the grade were walked");
        assert_eq!(
            skipped.len(),
            1,
            "one dead course is one skip for the grade, not one per section: {skipped:?}"
        );
        assert_eq!(skipped[0].reason, "course_deleted");
        assert_eq!(
            ClassBlueprint::read(blueprint.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_courses(),
            &[algebra.clone()],
            "and the list is pruned to the course that still exists"
        );
        for class in [&a, &b] {
            assert!(
                exists(
                    ClassCourseId::composite(class.get_id(), &algebra).record(),
                    &db
                )
                .await,
                "the live course still reached {}",
                class.get_name().as_str()
            );
        }
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let algebra = a_course("algebra", None, &db).await;
        a_section("9-A", &db).await;
        a_section("9-B", &db).await;
        let stale = a_blueprint(vec![algebra], &db).await;
        ClassBlueprint::read(stale.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .delete(&db)
            .await
            .unwrap();

        let Pumped { matched, skipped } = stale.pump(&manager, &db).await.unwrap();
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
            rows("SELECT VALUE id FROM class_course", &db).await,
            0,
            "…and nothing was attached under a blueprint nothing could sweep"
        );
    }

    #[test]
    fn a_grade_key_must_be_addressable() {
        assert!(ClassBlueprint::grade_key("").is_err());
        assert!(ClassBlueprint::grade_key("9/A").is_err());
        assert!(ClassBlueprint::grade_key("9%A").is_err());
        assert_eq!(
            ClassBlueprint::grade_key("9-A")
                .unwrap()
                .as_str()
                .to_string(),
            "9-A"
        );
        assert!(ClassBlueprint::grade_key(&"x".repeat(1000)).is_err());
    }

    /// The list is a *set*: a caller sending the same course twice must not
    /// make the pump attach it twice (the second is a duplicate anyway) nor
    /// spend two of the ceiling's places on one course.
    #[test]
    fn a_course_list_is_deduplicated_and_bounded() {
        let algebra = CourseId::from_key("algebra");
        let held = ClassBlueprint::course_list(vec![algebra.clone(), algebra.clone()]).unwrap();
        assert_eq!(held, vec![algebra]);

        let too_many: Vec<CourseId> = (0..=MAX_CLASS_COURSES)
            .map(|n| CourseId::from_key(&n.to_string()))
            .collect();
        assert!(ClassBlueprint::course_list(too_many).is_err());
    }

    /// A blueprint pumps best-effort, so every refusal the pump can answer must
    /// map to a reported skip rather than fall through as a success — and the
    /// two "nothing to do" answers must map to no skip at all.
    ///
    /// The codes are pinned exactly, not merely as "some": they are a published
    /// vocabulary a bilingual client branches and translates on, so a reworded
    /// one is a silent contract break. A new [`Attached`] variant fails the
    /// match arm above, and this pins what the existing ones say.
    ///
    /// Each one is also checked against [`Attached::refusal_code`], the manual
    /// routes' half of the same vocabulary: `duplicate` is the *only* place the
    /// two are allowed to differ, so a second spelling of any other code cannot
    /// drift in on either side.
    #[test]
    fn every_refusal_the_pump_can_answer_is_a_skip() {
        // `Made` is the other `None` there, and building one needs a live link
        // row — the arm itself is the only thing to check for it.
        assert!(skip_reason(&Attached::Duplicate).is_none());
        assert_eq!(
            Attached::<ClassCourse>::Duplicate.refusal_code(&Axis::Course),
            Some("duplicate"),
            "a hand attach's duplicate is a refusal even though a pump's is not"
        );
        for (refusal, code) in [
            (Attached::Gone, "class_deleted"),
            (Attached::PivotGone, "course_deleted"),
            (Attached::ClassFull, "class_at_course_ceiling"),
            (Attached::ClassOverloaded, "class_roster_too_large"),
            (Attached::Full("course:algebra".into()), "course_full"),
            (
                Attached::CourseGone("course:algebra".into()),
                "linked_course_missing",
            ),
            (Attached::SourceGone, "blueprint_deleted"),
        ] {
            assert_eq!(
                skip_reason(&refusal),
                Some(code),
                "{refusal:?} must be reported as its own code, not swallowed or reworded"
            );
            assert_eq!(
                refusal.refusal_code(&Axis::Course),
                skip_reason(&refusal),
                "{refusal:?} must read the same on a manual 409 as in a pump's skip list"
            );
        }
    }

    /// The two ceiling refusals mean the *opposite* ceiling on the two axes —
    /// `ClassFull` is the axis being attached, `ClassOverloaded` the other one —
    /// so the member add's pair must be the member add's own, and a code that
    /// reads the same on both axes is the bug this pins: the roster being full
    /// was published as `class_at_course_ceiling`.
    #[test]
    fn each_axis_names_the_ceiling_it_actually_hit() {
        for (refusal, course_axis, member_axis) in [
            (
                Attached::<ClassCourse>::ClassFull,
                "class_at_course_ceiling",
                "class_at_roster_ceiling",
            ),
            (
                Attached::ClassOverloaded,
                "class_roster_too_large",
                "class_course_list_too_large",
            ),
        ] {
            assert_eq!(refusal.refusal_code(&Axis::Course), Some(course_axis));
            assert_eq!(refusal.refusal_code(&Axis::Member), Some(member_axis));
        }
        // Every other code is axis-free, and must stay that way: they name a
        // record, not a ceiling.
        for refusal in [
            Attached::<ClassCourse>::Duplicate,
            Attached::Gone,
            Attached::PivotGone,
            Attached::Full("course:algebra".into()),
            Attached::CourseGone("course:algebra".into()),
            Attached::SourceGone,
        ] {
            assert_eq!(
                refusal.refusal_code(&Axis::Course),
                refusal.refusal_code(&Axis::Member),
                "{refusal:?} names a record, so it must read the same on both axes"
            );
        }
    }
}
