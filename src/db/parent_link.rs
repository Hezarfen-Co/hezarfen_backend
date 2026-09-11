//! The `parent_link` table: the composite-id upsert, the existence probe,
//! the per-parent listing, and the delete. The *decisions* over these rows
//! — who may be tied, and which reads a live link grants — live in
//! [`crate::service::parent_link`].

use crate::database::Database;
use crate::domain::parent_link::{ParentLink, ParentLinkId};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn link(
    db: &Database,
    parent: &UserId,
    student: &UserId,
    linked_by: &UserId,
) -> Result<ParentLink, AppError> {
    let link = ParentLink {
        id: ParentLinkId::composite(parent, student),
        parent: parent.clone(),
        student: student.clone(),
        linked_by: linked_by.clone(),
    };
    let saved: Option<ParentLink> = db.upsert(link.id.record()).content(link).await?;
    saved.ok_or_else(|| AppError::Internal("failed to link student to parent".into()))
}

/// True iff a link row exists for the (parent, student) pair — the storage
/// half of the grant. The live half (the student side still holding the
/// role) is [`crate::service::parent_link::links_live`].
pub async fn exists(db: &Database, parent: &UserId, student: &UserId) -> Result<bool, AppError> {
    let found: Option<ParentLink> = db
        .select(ParentLinkId::composite(parent, student).record())
        .await?;
    Ok(found.is_some())
}

/// Every student `parent` observes, newest link first.
pub async fn list_for_parent(db: &Database, parent: &UserId) -> Result<Vec<ParentLink>, AppError> {
    let mut result = db
        .query("SELECT * FROM parent_link WHERE parent = $parent ORDER BY id DESC")
        .bind(("parent", parent.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ParentLink>>(0)?)
}

pub async fn remove(
    db: &Database,
    parent: &UserId,
    student: &UserId,
) -> Result<Option<ParentLink>, AppError> {
    let mut result = db
        .query("DELETE parent_link WHERE parent = $parent AND student = $student RETURN BEFORE")
        .bind(("parent", parent.record()))
        .bind(("student", student.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ParentLink>>(0)?.into_iter().next())
}
