use sqlx::Type;
use uuid::Uuid;

use crate::constant::{MAX_MESSAGE_BODY_LEN, MAX_MESSAGE_LABEL_LEN, MAX_MESSAGE_SUBJECT_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

/// The mailbox folders a copy can sit in — the single source of folder names
/// for the whole stack. Nothing else may spell a folder: the DB columns, the
/// query filter and the restore stamp all pass through this enum, so a typo
/// is a compile error rather than a silently empty folder.
///
/// Each variant stores as a bare lowercase TEXT value (`"inbox"`,
/// `"archive"`, …) and round-trips straight back — same trick as
/// `domain::role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Folder {
    Inbox,
    Sent,
    Archive,
    Trash,
    /// The hidden "my copy is gone" folder; never a valid move or list
    /// target — reached only through [`crate::db::message::delete_for`].
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
    pub(crate) fn is_filed(self) -> bool {
        matches!(self, Folder::Archive | Folder::Trash)
    }

    /// Whether this folder is worth remembering as a restore target. The
    /// trash is not: pulling a copy out of the trash and into the archive
    /// must not leave it pointing back at the trash.
    pub(crate) fn is_restore_target(self) -> bool {
        matches!(self, Folder::Inbox | Folder::Sent | Folder::Archive)
    }
}

/// Folders a sender may file their side into (`Sent` is their home).
pub const SENDER_FOLDERS: [Folder; 3] = [Folder::Sent, Folder::Archive, Folder::Trash];
/// Folders a recipient may file their side into (`Inbox` is their home).
pub const RECIPIENT_FOLDERS: [Folder; 3] = [Folder::Inbox, Folder::Archive, Folder::Trash];

/// Typed message row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct MessageId(Uuid);

impl MessageId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// the folder listings sort `id DESC` (newest first,
    /// [`crate::db::message::list_folder`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Message {
    pub(crate) id: MessageId,
    pub(crate) sender: UserId,
    pub(crate) recipient: UserId,
    pub(crate) subject: MessageSubject,
    pub(crate) body: MessageBody,
    pub(crate) label: Option<MessageLabel>,
    pub(crate) sent_at: Timestamp,
    pub(crate) read: bool,
    pub(crate) sender_folder: Folder,
    pub(crate) recipient_folder: Folder,
    /// Where each side's copy sat before it was filed into archive/trash —
    /// the restore target. `None` while the copy sits in its home folder.
    pub(crate) sender_origin: Option<Folder>,
    pub(crate) recipient_origin: Option<Folder>,
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
        let sender = UserId::from_key("018f1a00-0000-7000-8000-000000000001");
        let recipient = UserId::from_key("018f1a00-0000-7000-8000-000000000002");
        let message = Message {
            id: MessageId::generate(),
            sender,
            recipient,
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

    #[test]
    fn sqlx_encodes_the_storage_form() {
        // The folder columns are TEXT with a CHECK on these exact words; the
        // sqlx encoding must never drift from `as_str`.
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for folder in [
            Folder::Inbox,
            Folder::Sent,
            Folder::Archive,
            Folder::Trash,
            Folder::Deleted,
        ] {
            buf.clear();
            let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&folder, &mut buf).unwrap();
            assert_eq!(std::str::from_utf8(&buf).unwrap(), folder.as_str());
        }
    }
}
