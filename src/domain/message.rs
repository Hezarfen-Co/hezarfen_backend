//! One-to-one mail-style messages. A single row serves both ends: the
//! recipient's `read` flag plus one folder field per side, so each party
//! files (archive/trash) and deletes their copy without touching the
//! other's. A side that permanently deletes goes to the hidden `deleted`
//! folder; once both sides are `deleted` the row itself is removed.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_MESSAGE_BODY_LEN, MAX_MESSAGE_LABEL_LEN, MAX_MESSAGE_SUBJECT_LEN};
use crate::database::{Database, MESSAGE_TABLE};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

/// Folders a sender may file their side into (`sent` is the default).
pub const SENDER_FOLDERS: [&str; 2] = ["sent", "trash"];
/// Folders a recipient may file their side into (`inbox` is the default).
pub const RECIPIENT_FOLDERS: [&str; 3] = ["inbox", "archive", "trash"];
/// The hidden "my copy is gone" folder; never a valid move target — reached
/// only through [`Message::delete_for`].
const DELETED_FOLDER: &str = "deleted";

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MessageId(RecordId);

impl MessageId {
    pub fn generate() -> Self {
        Self(RecordId::new(MESSAGE_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(MESSAGE_TABLE, key))
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
pub struct MessageSubject(String);

impl MessageSubject {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("subject", value, MAX_MESSAGE_SUBJECT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MessageBody(String);

impl MessageBody {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("body", value, MAX_MESSAGE_BODY_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A sender-chosen tag ("Etüt", "Sınav", …) the UI renders as a badge.
/// Required-and-bounded here; "no label" is `Option<MessageLabel>` on the row.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MessageLabel(String);

impl MessageLabel {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("label", value, MAX_MESSAGE_LABEL_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Message {
    id: MessageId,
    sender: UserId,
    recipient: UserId,
    subject: MessageSubject,
    body: MessageBody,
    label: Option<MessageLabel>,
    sent_at: Timestamp,
    read: bool,
    sender_folder: String,
    recipient_folder: String,
}

impl Message {
    pub fn get_id(&self) -> &MessageId {
        &self.id
    }

    pub fn get_sender(&self) -> &UserId {
        &self.sender
    }

    pub fn get_recipient(&self) -> &UserId {
        &self.recipient
    }

    pub fn get_subject(&self) -> &MessageSubject {
        &self.subject
    }

    pub fn get_body(&self) -> &MessageBody {
        &self.body
    }

    pub fn get_label(&self) -> Option<&MessageLabel> {
        self.label.as_ref()
    }

    pub fn get_sent_at(&self) -> Timestamp {
        self.sent_at
    }

    pub fn is_read(&self) -> bool {
        self.read
    }

    pub fn is_sender(&self, user: &UserId) -> bool {
        &self.sender == user
    }

    /// The folder `user`'s side of this message currently sits in.
    pub fn folder_of(&self, user: &UserId) -> &str {
        if self.is_sender(user) {
            &self.sender_folder
        } else {
            &self.recipient_folder
        }
    }

    pub async fn send(
        sender: &UserId,
        recipient: &UserId,
        subject: MessageSubject,
        body: MessageBody,
        label: Option<MessageLabel>,
        db: &Database,
    ) -> Result<Message, AppError> {
        let message = Message {
            id: MessageId::generate(),
            sender: sender.clone(),
            recipient: recipient.clone(),
            subject,
            body,
            label,
            sent_at: Timestamp::now(),
            read: false,
            sender_folder: "sent".to_string(),
            recipient_folder: "inbox".to_string(),
        };
        let created: Option<Message> = db.create(message.id.record()).content(message).await?;
        created.ok_or_else(|| AppError::Internal("failed to create message".into()))
    }

    /// One folder view for `user`, newest first. `inbox`/`archive` are
    /// recipient-side, `sent` is sender-side, `trash` is the union of both
    /// sides' trashed copies. `read` narrows to that flag state (`false` on
    /// the inbox is the unread view; on `sent`, receipts pending) — the
    /// folder condition is parenthesized because trash's is an OR.
    pub async fn list_folder(
        user: &UserId,
        folder: &str,
        read: Option<bool>,
        db: &Database,
    ) -> Result<Vec<Message>, AppError> {
        let condition = match folder {
            "sent" => "sender = $usr AND sender_folder = 'sent'",
            "trash" => {
                "(recipient = $usr AND recipient_folder = 'trash') \
                 OR (sender = $usr AND sender_folder = 'trash')"
            }
            _ => "recipient = $usr AND recipient_folder = $folder",
        };
        let read_clause = if read.is_some() {
            " AND read = $read"
        } else {
            ""
        };
        let mut result = db
            .query(format!(
                "SELECT * FROM message WHERE ({condition}){read_clause} ORDER BY id DESC"
            ))
            .bind(("usr", user.record()))
            .bind(("folder", folder.to_string()))
            .bind(("read", read.unwrap_or_default()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Message>>(0)?)
    }

    /// Read a message only if `user` is its sender or recipient and their
    /// side hasn't been permanently deleted.
    pub async fn read_for(
        id: &MessageId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Message>, AppError> {
        let message: Option<Message> = db.select(id.record()).await?;
        Ok(message.filter(|message| {
            (&message.sender == user || &message.recipient == user)
                && message.folder_of(user) != DELETED_FOLDER
        }))
    }

    /// Flip the recipient's read flag. Field-scoped write: the sender may be
    /// filing their side concurrently, and a whole-row save would clobber it.
    pub async fn set_read(self, read: bool, db: &Database) -> Result<Message, AppError> {
        let mut result = db
            .query("UPDATE $id SET read = $read RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("read", read))
            .await?
            .check()?;
        result
            .take::<Vec<Message>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// File `user`'s side into `folder` (already validated against that
    /// side's allowed set). Field-scoped for the same race reason as
    /// [`Message::set_read`].
    pub async fn move_to(
        self,
        user: &UserId,
        folder: &str,
        db: &Database,
    ) -> Result<Message, AppError> {
        let field = if self.is_sender(user) {
            "sender_folder"
        } else {
            "recipient_folder"
        };
        let mut result = db
            .query(format!("UPDATE $id SET {field} = $folder RETURN AFTER"))
            .bind(("id", self.id.record()))
            .bind(("folder", folder.to_string()))
            .await?
            .check()?;
        result
            .take::<Vec<Message>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Permanently drop `user`'s side — only if that side currently sits in
    /// the trash; the gate rides in the `UPDATE`'s `WHERE` so a concurrent
    /// move back to the inbox can't slip past a check-then-act window. The
    /// row itself is removed once both sides are gone — judged on the
    /// post-write row, so two concurrent deletes can't leak an all-deleted
    /// row.
    pub async fn delete_for(self, user: &UserId, db: &Database) -> Result<(), AppError> {
        let field = if self.is_sender(user) {
            "sender_folder"
        } else {
            "recipient_folder"
        };
        let mut result = db
            .query(format!(
                "UPDATE $id SET {field} = $deleted WHERE {field} = $trash RETURN AFTER"
            ))
            .bind(("id", self.id.record()))
            .bind(("deleted", DELETED_FOLDER.to_string()))
            .bind(("trash", "trash".to_string()))
            .await?
            .check()?;
        let Some(after) = result.take::<Vec<Message>>(0)?.into_iter().next() else {
            return Err(AppError::Conflict(
                "only messages in the trash can be permanently deleted",
            ));
        };
        if after.sender_folder == DELETED_FOLDER && after.recipient_folder == DELETED_FOLDER {
            let _: Option<Message> = db.delete(after.id.record()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subject_is_required_body_is_not() {
        assert_eq!(MessageSubject::try_new("hi").unwrap().as_str(), "hi");
        assert!(MessageSubject::try_new("  ").is_err());
        assert_eq!(MessageBody::try_new("").unwrap().as_str(), "");
    }

    #[tokio::test]
    async fn folder_sides_resolve_by_membership() {
        let sender = UserId::from_key("a");
        let recipient = UserId::from_key("b");
        let message = Message {
            id: MessageId::generate(),
            sender: sender.clone(),
            recipient: recipient.clone(),
            subject: MessageSubject::try_new("s").unwrap(),
            body: MessageBody::try_new("").unwrap(),
            label: None,
            sent_at: Timestamp::now(),
            read: false,
            sender_folder: "sent".to_string(),
            recipient_folder: "archive".to_string(),
        };
        assert!(message.is_sender(&sender));
        assert_eq!(message.folder_of(&sender), "sent");
        assert_eq!(message.folder_of(&recipient), "archive");
    }
}
