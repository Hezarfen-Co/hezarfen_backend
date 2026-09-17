//! Instance visibility: the two questions every instance-scoped surface asks
//! before it reads or writes anything — whether this caller may act on one
//! instance, and which instances are theirs at all. Both lived in
//! [`crate::web::instances`] while the HTTP layer was their only caller; the
//! RAG scope deriver ([`super::rag_scope`]) asks the same two questions, and a
//! service may not reach up into the web layer — nor grow a second copy of a
//! rule that two surfaces must never disagree about.
//!
//! The D10 rule itself is [`super::class_course::ensure_instance_teacher`],
//! which [`can_manage_instance`] asks as a boolean so a route can word its own
//! `403`; the row reads are [`crate::db`]'s.

use std::collections::HashSet;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::class_group::ClassGroupId;
use crate::domain::role::Role;
use crate::domain::user::User;
use crate::error::AppError;

/// Whether `user` may act inside this instance — D10, asked as a boolean so a
/// handler can word its own `403` (the one gate every instance-scoped route
/// shares; [`super::class_course::ensure_instance_teacher`] is the same rule
/// with the refusal in it).
pub async fn can_manage_instance(
    db: &Database,
    instance: &ClassCourseId,
    user: &User,
) -> Result<bool, AppError> {
    Ok(
        super::class_course::ensure_instance_teacher(db, user, instance)
            .await
            .is_ok(),
    )
}

/// The instances a caller may see, each with whether they run it: the
/// instances their şubeler carry (a student's roster, and a homeroom
/// teacher's), the instances they were assigned to teach (a teacher), and —
/// for a homeroom teacher — their own şube's. The per-instance answer is the
/// same D10 rule every instance-scoped gate applies, computed once for the
/// list.
///
/// [`crate::web::instances`] serves this set to every caller as
/// `GET /instances/me`; the catalog-wide list other instance routes hand a
/// manager+ is [`crate::web::courses`]'. [`super::rag_scope`] scopes a RAG
/// question by it, which is why it lives here now.
///
/// Staff are *not* members of the section they run, so the two ways a teacher
/// reaches a şube are read separately and unioned: `class_member` (a student's
/// live stint) and the homeroom column (`class_group.teacher`, read through
/// [`super::class_group::list_for_teacher`]). Leaving the second out dropped
/// every row for a homeroom teacher who was neither enrolled nor assigned —
/// even though [`super::class_course::ensure_instance_teacher`] lets them act
/// on all of them.
pub async fn visible_instances(
    user: &User,
    db: &Database,
) -> Result<Vec<(ClassCourse, bool)>, AppError> {
    let mut rows: Vec<(ClassCourse, bool)> = Vec::new();
    // The şubeler the caller is a live member of, unioned with the ones they
    // are the homeroom teacher of (an empty second read for a student, and a
    // cheap one for anyone else — it is keyed on the teacher column).
    //
    // The homeroom half carries the live-`teacher` floor the whole gate does
    // (see [`super::class_course::ensure_instance_teacher`]): the column is
    // history, a role flip that never ran the cascade leaves it standing, and
    // an account below `teacher` may not keep reaching its sections — nor have
    // the `manages` flag that shows them the drafts.
    let staff = user.get_role().at_least(Role::Teacher);
    let (members, _) = crate::db::class_member::list_for_user(db, user.get_id(), None, 0).await?;
    let mut classes: Vec<ClassGroupId> = members
        .iter()
        .map(|member| member.get_class().clone())
        .collect();
    let homeroom = if staff {
        super::class_group::list_for_teacher(db, user.get_id()).await?
    } else {
        Vec::new()
    };
    let homeroom_keys: Vec<String> = homeroom.iter().map(|class| class.get_id().key()).collect();
    let mut seen: HashSet<String> = classes.iter().map(|class| class.key()).collect();
    classes.extend(
        homeroom
            .iter()
            .map(|class| class.get_id().clone())
            .filter(|class| seen.insert(class.key())),
    );
    for instance in crate::db::class_course::list_for_class_ids(db, &classes).await? {
        let manages = homeroom_keys.contains(&instance.get_class().key());
        rows.push((instance, manages));
    }
    // The instances the caller teaches: read through the catalog courses they
    // are assigned in (the junction has no teacher-keyed read of its own) and
    // kept only where the assignment actually names them.
    if user.get_role().at_least(Role::Teacher) {
        for course in super::course::list_for_teacher(db, user.get_id()).await? {
            let (taught, _) =
                crate::db::class_course::list_for_course(db, course.get_id(), None, 0).await?;
            for instance in taught {
                if rows
                    .iter()
                    .any(|(row, _)| row.get_id() == instance.get_id())
                {
                    continue;
                }
                if can_manage_instance(db, instance.get_id(), user).await? {
                    rows.push((instance, true));
                }
            }
        }
    }
    Ok(rows)
}
