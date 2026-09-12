//! The `parent_link` table: the composite-PK upsert, the existence probe,
//! the per-parent listing, and the delete. The *decisions* over these rows
//! — who may be tied, and which reads a live link grants — live in
//! [`crate::service::parent_link`].

use crate::database::Database;
use crate::domain::parent_link::ParentLink;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

pub async fn link(
    db: &Database,
    parent: &UserId,
    student: &UserId,
    linked_by: &UserId,
) -> Result<ParentLink, AppError> {
    // The pair is the primary key, so the upsert is one atomic statement —
    // no find-then-insert race to lose, and a re-link restamps `linked_by`.
    let link = query_as!(
        ParentLink,
        "INSERT INTO parent_link (parent, student, linked_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (parent, student) DO UPDATE SET linked_by = $3
         RETURNING parent, student, linked_by",
        parent,
        student,
        linked_by
    )
    .fetch_one(db)
    .await?;
    Ok(link)
}

/// True iff a link row exists for the (parent, student) pair — the storage
/// half of the grant. The live half (the student side still holding the
/// role) is [`crate::service::parent_link::links_live`].
pub async fn exists(db: &Database, parent: &UserId, student: &UserId) -> Result<bool, AppError> {
    let row = sqlx::query!(
        "SELECT EXISTS(SELECT 1 FROM parent_link WHERE parent = $1 AND student = $2) AS present",
        parent,
        student
    )
    .fetch_one(db)
    .await?;
    Ok(row.present)
}

/// Every student `parent` observes. The composite key's text form ordered
/// the old listing, which for one parent means the student ids — the same
/// order on the natural key.
pub async fn list_for_parent(db: &Database, parent: &UserId) -> Result<Vec<ParentLink>, AppError> {
    let links = query_as!(
        ParentLink,
        "SELECT parent, student, linked_by FROM parent_link WHERE parent = $1
         ORDER BY student DESC",
        parent
    )
    .fetch_all(db)
    .await?;
    Ok(links)
}

pub async fn remove(
    db: &Database,
    parent: &UserId,
    student: &UserId,
) -> Result<Option<ParentLink>, AppError> {
    let gone = query_as!(
        ParentLink,
        "DELETE FROM parent_link WHERE parent = $1 AND student = $2
         RETURNING parent, student, linked_by",
        parent,
        student
    )
    .fetch_optional(db)
    .await?;
    Ok(gone)
}
