//! One-to-one mail-style messages. A single row serves both ends: the
//! recipient's `read` flag plus one folder field per side, so each party
//! files (archive/trash) and deletes their copy without touching the
//! other's. Filing also stamps that side's origin folder, so a copy can be
//! put back where it came from. A side that permanently deletes goes to the
//! hidden `deleted` folder; once both sides are `deleted` the row itself is
//! removed.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    MAX_MESSAGE_BODY_LEN, MAX_MESSAGE_LABEL_LEN, MAX_MESSAGE_SUBJECT_LEN, MESSAGE_TABLE,
    RECIPIENT_FOLDERS, SENDER_FOLDERS,
};
use crate::database::Database;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

/// The mailbox folders a copy can sit in — the single source of folder names
/// for the whole stack. Nothing else may spell a folder: the DB columns, the
/// query filter and the restore stamp all pass through this enum, so a typo
/// is a compile error rather than a silently empty folder.
///
/// `#[surreal(untagged, rename_all = "lowercase")]` stores each variant as a
/// bare lowercase string (`"inbox"`, `"archive"`, …) in the `TYPE string`
/// columns, and round-trips straight back — same trick as `domain::role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum Folder {
    Inbox,
    Sent,
    Archive,
    Trash,
    /// The hidden "my copy is gone" folder; never a valid move or list
    /// target — reached only through [`Message::delete_for`].
    Deleted,
}

impl Folder {
    /// The storage/wire form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            Folder::Inbox => "inbox",
            Folder::Sent => "sent",
            Folder::Archive => "archive",
            Folder::Trash => "trash",
            Folder::Deleted => "deleted",
        }
    }

    /// Parse a caller-supplied folder name. `deleted` is internal, so it is
    /// rejected here like any other unknown word — a caller can never name it.
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        match value {
            "inbox" => Ok(Folder::Inbox),
            "sent" => Ok(Folder::Sent),
            "archive" => Ok(Folder::Archive),
            "trash" => Ok(Folder::Trash),
            _ => Err(ValidationError::Invalid {
                field: "folder",
                reason: "must be one of: inbox, sent, archive, trash",
            }),
        }
    }

    /// Where a side's copy lives when nothing has been filed away.
    pub fn home(is_sender: bool) -> Self {
        if is_sender {
            Folder::Sent
        } else {
            Folder::Inbox
        }
    }

    /// The folders a side may move its copy to.
    pub fn allowed_for(is_sender: bool) -> &'static [Folder] {
        if is_sender {
            &SENDER_FOLDERS
        } else {
            &RECIPIENT_FOLDERS
        }
    }

    /// A filed-away copy — one that has somewhere to be restored to.
    fn is_filed(self) -> bool {
        matches!(self, Folder::Archive | Folder::Trash)
    }

    /// Whether this folder is worth remembering as a restore target. The
    /// trash is not: pulling a copy out of the trash and into the archive
    /// must not leave it pointing back at the trash.
    fn is_restore_target(self) -> bool {
        matches!(self, Folder::Inbox | Folder::Sent | Folder::Archive)
    }
}

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
    sender_folder: Folder,
    recipient_folder: Folder,
    /// Where each side's copy sat before it was filed into archive/trash —
    /// the restore target. `None` while the copy sits in its home folder.
    sender_origin: Option<Folder>,
    recipient_origin: Option<Folder>,
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
    pub fn folder_of(&self, user: &UserId) -> Folder {
        if self.is_sender(user) {
            self.sender_folder
        } else {
            self.recipient_folder
        }
    }

    /// Where `user`'s copy came from before it was filed away — `None` when
    /// it isn't filed away (or was filed by a binary older than the memory),
    /// in which case restoring means [`Folder::home`].
    pub fn origin_of(&self, user: &UserId) -> Option<Folder> {
        if self.is_sender(user) {
            self.sender_origin
        } else {
            self.recipient_origin
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
            sender_folder: Folder::Sent,
            recipient_folder: Folder::Inbox,
            sender_origin: None,
            recipient_origin: None,
        };
        let created: Option<Message> = db.create(message.id.record()).content(message).await?;
        created.ok_or_else(|| AppError::Internal("failed to create message".into()))
    }

    /// One folder view for `user`, newest first. `inbox` is recipient-side,
    /// `sent` is sender-side, and `archive`/`trash` are each the union of both
    /// sides' filed copies (either party may archive or trash their own copy).
    /// `read` narrows to that flag state (`false` on the inbox is the unread
    /// view; on `sent`, receipts pending) — the folder condition is
    /// parenthesized because `archive`'s and `trash`'s are ORs.
    pub async fn list_folder(
        user: &UserId,
        folder: Folder,
        read: Option<bool>,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Message>, i64), AppError> {
        let condition = match folder {
            Folder::Sent => "sender = $usr AND sender_folder = 'sent'",
            Folder::Archive => {
                "(recipient = $usr AND recipient_folder = 'archive') \
                 OR (sender = $usr AND sender_folder = 'archive')"
            }
            Folder::Trash => {
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
        PagedList::new(
            format!("message WHERE ({condition}){read_clause}"),
            "ORDER BY id DESC",
        )
        .bind("usr", user.record())
        .bind("folder", folder.as_str().to_string())
        .bind("read", read.unwrap_or_default())
        .run(limit, offset, db)
        .await
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
                && message.folder_of(user) != Folder::Deleted
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
    /// side's allowed set), stamping where the copy came from so it can be
    /// restored. Only the caller's two fields are written — field-scoped for
    /// the same race reason as [`Message::set_read`].
    ///
    /// Filing away remembers the folder being left, so `inbox → archive →
    /// trash` restores to `archive` and that back to `inbox`. Two folders are
    /// never stamped: the trash (`trash → archive` would leave the copy
    /// pointing back at the discard pile — it falls back to the home folder)
    /// and the target itself (a no-op move keeps the memory it had). Moving
    /// back to a home folder clears it.
    pub async fn move_to(
        self,
        user: &UserId,
        folder: Folder,
        db: &Database,
    ) -> Result<Message, AppError> {
        let current = self.folder_of(user);
        let (field, origin_field) = if self.is_sender(user) {
            ("sender_folder", "sender_origin")
        } else {
            ("recipient_folder", "recipient_origin")
        };
        let origin = match folder {
            _ if !folder.is_filed() => None,
            _ if current == folder => self.origin_of(user),
            _ if current.is_restore_target() => Some(current),
            _ => None,
        };
        let mut result = db
            .query(format!(
                "UPDATE $id SET {field} = $folder, {origin_field} = $origin RETURN AFTER"
            ))
            .bind(("id", self.id.record()))
            .bind(("folder", folder.as_str().to_string()))
            .bind(("origin", origin.map(|origin| origin.as_str().to_string())))
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
        let (field, origin_field) = if self.is_sender(user) {
            ("sender_folder", "sender_origin")
        } else {
            ("recipient_folder", "recipient_origin")
        };
        let mut result = db
            .query(format!(
                "UPDATE $id SET {field} = $deleted, {origin_field} = NONE \
                 WHERE {field} = $trash RETURN AFTER"
            ))
            .bind(("id", self.id.record()))
            .bind(("deleted", Folder::Deleted.as_str().to_string()))
            .bind(("trash", Folder::Trash.as_str().to_string()))
            .await?
            .check()?;
        let Some(after) = result.take::<Vec<Message>>(0)?.into_iter().next() else {
            return Err(AppError::Conflict(
                "only messages in the trash can be permanently deleted",
            ));
        };
        if after.sender_folder == Folder::Deleted && after.recipient_folder == Folder::Deleted {
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
            sender_folder: Folder::Sent,
            recipient_folder: Folder::Archive,
            sender_origin: None,
            recipient_origin: Some(Folder::Inbox),
        };
        assert!(message.is_sender(&sender));
        assert_eq!(message.folder_of(&sender), Folder::Sent);
        assert_eq!(message.folder_of(&recipient), Folder::Archive);
        assert_eq!(message.origin_of(&sender), None);
        assert_eq!(message.origin_of(&recipient), Some(Folder::Inbox));
    }

    #[tokio::test]
    async fn folder_names_parse_and_the_hidden_one_does_not() {
        assert_eq!(Folder::try_new("archive").unwrap(), Folder::Archive);
        assert!(Folder::try_new("deleted").is_err());
        assert!(Folder::try_new("spam").is_err());
        assert_eq!(Folder::home(true), Folder::Sent);
        assert_eq!(Folder::home(false), Folder::Inbox);
        // Both sides may archive their own copy; neither may reach the inbox
        // that isn't theirs — the sender's home is `sent`, not `inbox`.
        assert!(Folder::allowed_for(true).contains(&Folder::Archive));
        assert!(Folder::allowed_for(false).contains(&Folder::Archive));
        assert!(!Folder::allowed_for(true).contains(&Folder::Inbox));
    }
}
