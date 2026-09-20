//! The `(class, course)` scope a RAG question is answered from.
//!
//! The RAG corpus is routed by the **pair** — a grade and a subject list sent
//! separately would cross-product into combinations the asker never named
//! ([`crate::constant::MAX_RAG_SCOPE_PAIRS`]) — so every question needs the set
//! of pairs its asker is allowed to retrieve from, and that set is derived
//! *here*, from the asker's own rows, never from the request body: a scope a
//! client could name is a scope a client could widen.
//!
//! The derivation is the school's own visibility rules, not a second copy of
//! them. A student's or a teacher's pairs are the courses their
//! [`super::instance::visible_instances`] teach them — the very set
//! `GET /instances/me` serves — and a club/supervised-study membership
//! ([`crate::db::course_membership`]) adds its subject with no grade, since a
//! school-scoped course belongs to no section. A parent has no sections of
//! their own, so their scope is their linked children's, read child by child
//! through the same reader: what a parent may ask about is exactly what their
//! children could be taught.
//!
//! Two consequences are deliberate. An empty scope is legal and returned as an
//! empty list — the service answers an unscopable question by abstaining, and a
//! `400` here would refuse questions the asker is entitled to ask. And a union
//! over the cap is **refused**, never narrowed: dropping pairs would answer a
//! question scoped to corpora the asker did not name, which is worse than no
//! answer at all.

use std::collections::{HashMap, HashSet};

use crate::ai::rag_chat::RagScopePair;
use crate::constant::MAX_RAG_SCOPE_PAIRS;
use crate::database::Database;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::role::Role;
use crate::domain::user::User;
use crate::error::{AppError, ValidationError};

/// The `(class, course)` pairs `user`'s questions may be scoped to, in
/// first-seen order and deduped — one entry per distinct pair.
///
/// `role` is the caller's **live** session role, the same value the
/// `rag.chat` payload carries: the instance arm is the asker's own rows for
/// everyone but a parent (a section membership, a homeroom column and a
/// teaching assignment are all keyed on live roles anyway), and the parent
/// arm is the one that has to be chosen by role, because a parent's own
/// sections do not exist.
///
/// The instance arm reads its rows through [`super::instance::visible_instances`]
/// and the two lookup tables (the class sections the pairs sit in and the
/// titles of every course named) in one batch read each — never one query per
/// instance: a teacher can be assigned to dozens of sections, and a per-row
/// lookup would spend the request's whole budget before the AI service is even
/// called.
pub async fn for_user(
    db: &Database,
    user: &User,
    role: Role,
) -> Result<Vec<RagScopePair>, AppError> {
    let mut instances: Vec<ClassCourse> = Vec::new();
    if role == Role::Parent {
        // A parent's scope is what their children are taught — read through
        // the same visibility rule, one child at a time because the rule
        // keys on the person asking. A link whose student side changed role
        // is inert by construction: the demotion sweep takes their
        // memberships and enrollments with it, so the child simply has
        // nothing left to widen the parent into.
        for link in crate::db::parent_link::list_for_parent(db, user.get_id()).await? {
            let Some(child) = super::user::read(db, link.get_student()).await? else {
                continue;
            };
            instances.extend(
                super::instance::visible_instances(&child, db)
                    .await?
                    .into_iter()
                    .map(|(instance, _)| instance),
            );
        }
    } else {
        instances.extend(
            super::instance::visible_instances(user, db)
                .await?
                .into_iter()
                .map(|(instance, _)| instance),
        );
    }
    // The school-scoped courses the asker joined directly (club, supervised
    // study). Every role may hold one — the table's own gate is the student
    // role, so for staff this read is simply empty.
    let memberships =
        crate::db::course_membership::list_for_user(db, user.get_id(), None, 0)
            .await?
            .0;

    // One batch read per lookup, over every id the two arms named: the class
    // sections decide each instance's grade, and the courses carry the titles
    // every pair is scoped by. Ids repeat freely — a class usually carries
    // several instances — and `list_by_ids` answers each row once.
    let class_ids: Vec<ClassGroupId> = instances
        .iter()
        .map(|instance| instance.get_class().clone())
        .collect();
    let mut course_ids: Vec<CourseId> = instances
        .iter()
        .map(|instance| instance.get_course().clone())
        .collect();
    course_ids.extend(
        memberships
            .iter()
            .map(|membership| membership.get_course().clone()),
    );
    let classes = crate::db::class_group::list_by_ids(db, &class_ids).await?;
    let courses = crate::db::course::list_by_ids(db, &course_ids).await?;

    // Grade labels, keyed by class section. A class with no grade at all (a
    // club-shaped section, and the shape a fixture class carries) simply
    // contributes no entry, so its instances scope the subject across every
    // grade — the documented meaning of an absent `sinif`.
    let grades: HashMap<String, String> = classes
        .iter()
        .filter_map(|class| {
            class
                .get_grade()
                .map(|grade| (class.get_id().key(), grade.as_str().to_owned()))
        })
        .collect();
    let titles: HashMap<String, String> = courses
        .iter()
        .map(|course| (course.get_id().key(), course.get_title().as_str().to_owned()))
        .collect();

    let mut pairs: Vec<RagScopePair> = Vec::new();
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();
    for instance in &instances {
        // A course row the batch read did not answer is unreachable — the
        // instance's key is a foreign key — so the skip is a guard against a
        // dangling read, not a rule: a title-less pair could not be routed.
        let Some(ders) = titles.get(&instance.get_course().key()) else {
            continue;
        };
        push_pair(
            &mut pairs,
            &mut seen,
            grades.get(&instance.get_class().key()).cloned(),
            ders.clone(),
        );
    }
    for membership in &memberships {
        let Some(ders) = titles.get(&membership.get_course().key()) else {
            continue;
        };
        push_pair(&mut pairs, &mut seen, None, ders.clone());
    }
    // The cap is checked on the *union*: the dedup is what one frame would
    // carry, and it is exactly the number this refusal is about. Refused,
    // never truncated — see the module doc.
    if pairs.len() > MAX_RAG_SCOPE_PAIRS {
        return Err(AppError::Validation(ValidationError::TooLong {
            field: "scope",
            max: MAX_RAG_SCOPE_PAIRS,
            got: pairs.len(),
        }));
    }
    Ok(pairs)
}

/// Append `pair` unless the very same pair is already in the list — a scope is
/// a set. Two class sections at one grade teaching one course are one scope,
/// and a course attached to a section while also joined as a club contributes
/// both of its pairs; a duplicate would spend one of the cap's slots twice and
/// hand the service the same corpus under two entries.
fn push_pair(
    pairs: &mut Vec<RagScopePair>,
    seen: &mut HashSet<(Option<String>, String)>,
    sinif: Option<String>,
    ders: String,
) {
    if seen.insert((sinif.clone(), ders.clone())) {
        pairs.push(RagScopePair { sinif, ders });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::class_group::{ClassGrade, ClassGroupId, ClassName};
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::user::Username;

    /// One scope pair, spelled the way the service reads the wire type.
    fn pair(sinif: Option<&str>, ders: &str) -> RagScopePair {
        RagScopePair {
            sinif: sinif.map(str::to_owned),
            ders: ders.to_owned(),
        }
    }

    /// A real account at a role: every arm of the derivation judges the live
    /// row (and the membership and parent-link gates judge the exact role), so
    /// a fabricated id or the `fixture_user` default would not do.
    async fn person(db: &Database, username: &str, role: Role) -> User {
        crate::db::user::create_with_role(
            db,
            Username::try_new(username).unwrap(),
            None,
            role,
            None,
        )
        .await
        .unwrap()
    }

    /// A class section carrying a grade label — the half of every instance
    /// pair, since `a_class` deliberately has none.
    async fn graded_class(db: &Database, name: &str, grade: &str) -> ClassGroupId {
        let office = crate::db::class_member::tests::fixture_user(db, "ragscope-office").await;
        crate::service::class_group::create(
            db,
            &office,
            ClassName::try_new(name).unwrap(),
            Some(ClassGrade::try_new(grade).unwrap()),
            None,
            None,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// Attach a catalog course to a class section, answering the instance.
    async fn attach(
        db: &Database,
        class: &ClassGroupId,
        title: &str,
    ) -> crate::domain::class_course::ClassCourse {
        let office = crate::db::class_member::tests::fixture_user(db, "ragscope-office").await;
        let course = crate::db::class_member::tests::a_course(title, db).await;
        crate::service::class_course::attach(db, class, &course, &office)
            .await
            .unwrap()
    }

    /// A student at a section's roster — the membership the instance half of
    /// the scope is read through.
    async fn enrolment(db: &Database, class: &ClassGroupId, student: &User) {
        let office = crate::db::class_member::tests::fixture_user(db, "ragscope-office").await;
        crate::service::class_member::add(db, class, student.get_id(), &office)
            .await
            .unwrap();
    }

    /// A student's pair is the section's grade plus the course attached to
    /// it — the same set `GET /instances/me` hands them.
    #[tokio::test]
    async fn a_students_sections_scope_their_subjects() {
        let (db, _leases) = crate::database::init_test_db().await;
        let student = person(&db, "ragscope-student", Role::Student).await;
        let class = graded_class(&db, "5-A", "5").await;
        attach(&db, &class, "Matematik").await;
        enrolment(&db, &class, &student).await;

        let scope = for_user(&db, &student, Role::Student).await.unwrap();
        assert_eq!(scope, vec![pair(Some("5"), "Matematik")]);
    }

    /// A teacher reaches a section through the teaching assignment, not
    /// through a roster: the course they run is in scope exactly as it is for
    /// the students they teach.
    #[tokio::test]
    async fn an_assigned_teachers_subject_is_in_scope() {
        let (db, _leases) = crate::database::init_test_db().await;
        let teacher = person(&db, "ragscope-teacher", Role::Teacher).await;
        let class = graded_class(&db, "9-A", "9").await;
        let instance = attach(&db, &class, "Fizik").await;
        // The office staffing call minus its role gate: what this test reads
        // is the scope, not who may write the assignment row.
        crate::db::class_course_teacher::assign(&db, instance.get_id(), teacher.get_id())
            .await
            .unwrap();

        let scope = for_user(&db, &teacher, Role::Teacher).await.unwrap();
        assert_eq!(scope, vec![pair(Some("9"), "Fizik")]);
    }

    /// A parent has no sections of their own: their scope is the linked
    /// child's, and nobody else's — an unrelated student's section, graded the
    /// same or not, never widens it.
    #[tokio::test]
    async fn a_parents_scope_is_their_linked_childs() {
        let (db, _leases) = crate::database::init_test_db().await;
        let parent = person(&db, "ragscope-parent", Role::Parent).await;
        let child = person(&db, "ragscope-child", Role::Student).await;
        let stranger = person(&db, "ragscope-stranger", Role::Student).await;
        let child_class = graded_class(&db, "7-A", "7").await;
        attach(&db, &child_class, "Türkçe").await;
        enrolment(&db, &child_class, &child).await;
        let stranger_class = graded_class(&db, "7-B", "7").await;
        attach(&db, &stranger_class, "Türkçe").await;
        enrolment(&db, &stranger_class, &stranger).await;
        let office = crate::db::class_member::tests::fixture_user(&db, "ragscope-office").await;
        crate::service::parent_link::link(&db, parent.get_id(), child.get_id(), &office)
            .await
            .unwrap();

        let scope = for_user(&db, &parent, Role::Parent).await.unwrap();
        assert_eq!(
            scope,
            vec![pair(Some("7"), "Türkçe")],
            "the linked child's section, and only that one"
        );
    }

    /// A club or supervised study is school-scoped: it belongs to no class
    /// section, so its subject is scoped with no grade at all.
    #[tokio::test]
    async fn a_club_membership_scopes_a_subject_with_no_grade() {
        let (db, _leases) = crate::database::init_test_db().await;
        let student = person(&db, "ragscope-club", Role::Student).await;
        let office = crate::db::class_member::tests::fixture_user(&db, "ragscope-office").await;
        let club = crate::db::course::create(
            &db,
            &office,
            CourseTitle::try_new("Satranç").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("club").unwrap(),
        )
        .await
        .unwrap();
        crate::db::course_membership::add(&db, club.get_id(), student.get_id(), &office)
            .await
            .unwrap();

        let scope = for_user(&db, &student, Role::Student).await.unwrap();
        assert_eq!(scope, vec![pair(None, "Satranç")]);
    }

    /// The same pair reached twice is one scope: two class sections at one
    /// grade teaching one course are two instances and a single
    /// `(class, course)`, which is what the corpus is routed by.
    #[tokio::test]
    async fn a_pair_reached_twice_rides_once() {
        let (db, _leases) = crate::database::init_test_db().await;
        let student = person(&db, "ragscope-twice", Role::Student).await;
        let a = graded_class(&db, "6-A", "6").await;
        let b = graded_class(&db, "6-B", "6").await;
        attach(&db, &a, "Matematik").await;
        attach(&db, &b, "Matematik").await;
        enrolment(&db, &a, &student).await;
        enrolment(&db, &b, &student).await;

        // Both sections really are the student's — the pair was reached twice.
        assert_eq!(
            crate::service::instance::visible_instances(&student, &db)
                .await
                .unwrap()
                .len(),
            2
        );
        let scope = for_user(&db, &student, Role::Student).await.unwrap();
        assert_eq!(scope, vec![pair(Some("6"), "Matematik")]);
    }
}
