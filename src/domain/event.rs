use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_EVENT_DESCRIPTION_LEN, MAX_EVENT_TITLE_LEN};
use crate::database::{Database, EVENT_TABLE};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventId(RecordId);

impl EventId {
    pub fn generate() -> Self {
        Self(RecordId::new(EVENT_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EVENT_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventTitle(String);

impl EventTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_EVENT_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventDescription(String);

impl EventDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_EVENT_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Event {
    id: EventId,
    creator: UserId,
    title: EventTitle,
    description: EventDescription,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
}

impl Event {
    pub fn get_id(&self) -> &EventId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &EventTitle {
        &self.title
    }

    pub fn get_description(&self) -> &EventDescription {
        &self.description
    }

    pub fn get_starts_at(&self) -> Option<Timestamp> {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    pub async fn create(
        creator: &UserId,
        title: EventTitle,
        description: EventDescription,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<Event, AppError> {
        let event = Event {
            id: EventId::generate(),
            creator: creator.clone(),
            title,
            description,
            starts_at,
            ends_at,
        };
        let created: Option<Event> = db.create(event.id.record()).content(event).await?;
        created.ok_or_else(|| AppError::Internal("failed to create event".into()))
    }

    pub async fn read(id: &EventId, db: &Database) -> Result<Option<Event>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<Event>, AppError> {
        let mut result = db
            .query("SELECT * FROM event ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Event>>(0)?)
    }

    pub async fn update(
        mut self,
        title: EventTitle,
        description: EventDescription,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<Event, AppError> {
        self.title = title;
        self.description = description;
        self.starts_at = starts_at;
        self.ends_at = ends_at;
        let updated: Option<Event> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the event and cascade-remove its attendance rows.
    pub async fn delete(self, db: &Database) -> Result<Event, AppError> {
        db.query("DELETE attendance WHERE event = $ev")
            .bind(("ev", self.id.record()))
            .await?
            .check()?;
        let deleted: Option<Event> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(EventTitle::try_new("standup").is_ok());
        assert!(EventTitle::try_new("").is_err());
        assert!(EventTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(EventDescription::try_new("").is_ok());
    }
}
