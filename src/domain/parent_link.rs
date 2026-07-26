use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::PARENT_LINK_TABLE;
use crate::database::Database;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ParentLinkId(RecordId);

impl ParentLinkId {
    /// A deterministic id for the (parent, student) pair — same trick as
    /// `EnrollmentId`: the same pair always maps to the same record id, so
    /// linking is a single atomic UPSERT with no find-then-insert race and
    /// one-row-per-pair by construction.
    pub fn composite(parent: &UserId, student: &UserId) -> Self {
        Self(RecordId::new(
            PARENT_LINK_TABLE,
            format!("{}_{}", parent.key(), student.key()),
        ))
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

/// A parent account's tie to one student it may observe. Almost entirely a
/// read grant: it gates the parent's access to the student's reports (marks,
/// attendance, pomodoro). The single write it authorizes is the food program —
/// a parent books and cancels a linked child's meals, since paying for lunch
/// is a parent's job and a small child cannot do it themselves.
#[derive(Debug, Clone, SurrealValue)]
pub struct ParentLink {
    id: ParentLinkId,
    parent: UserId,
    student: UserId,
    linked_by: UserId,
}

impl ParentLink {
    pub fn get_id(&self) -> &ParentLinkId {
        &self.id
    }

    pub fn get_parent(&self) -> &UserId {
        &self.parent
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_linked_by(&self) -> &UserId {
        &self.linked_by
    }

    /// Tie (idempotently) `student` to `parent`. One row per pair, keyed by the
    /// deterministic composite id, so concurrent links converge on one row. The
    /// caller validates the roles — this only stores the pair.
    pub async fn link(
        parent: &UserId,
        student: &UserId,
        linked_by: &UserId,
        db: &Database,
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

    /// True iff `parent` holds a link to `student` — the observation gate.
    pub async fn exists(
        parent: &UserId,
        student: &UserId,
        db: &Database,
    ) -> Result<bool, AppError> {
        let found: Option<ParentLink> = db
            .select(ParentLinkId::composite(parent, student).record())
            .await?;
        Ok(found.is_some())
    }

    /// Every student `parent` observes, newest link first.
    pub async fn list_for_parent(
        parent: &UserId,
        db: &Database,
    ) -> Result<Vec<ParentLink>, AppError> {
        let mut result = db
            .query("SELECT * FROM parent_link WHERE parent = $parent ORDER BY id DESC")
            .bind(("parent", parent.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ParentLink>>(0)?)
    }

    pub async fn remove(
        parent: &UserId,
        student: &UserId,
        db: &Database,
    ) -> Result<Option<ParentLink>, AppError> {
        let mut result = db
            .query("DELETE parent_link WHERE parent = $parent AND student = $student RETURN BEFORE")
            .bind(("parent", parent.record()))
            .bind(("student", student.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ParentLink>>(0)?.into_iter().next())
    }

    /// Drop every link where `user` is the observed student — called when a
    /// role change takes the student side out of the `student` role, mirroring
    /// the enrollment sweep.
    pub async fn delete_where_student(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE parent_link WHERE student = $usr")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Drop every link where `user` is the observing parent — called when a
    /// role change takes the parent side out of the `parent` role.
    pub async fn delete_where_parent(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE parent_link WHERE parent = $usr")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }
}
