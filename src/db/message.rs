//! The `message` table: the send-side create, the folder listings, the
//! party-scoped read, and the field-scoped writes each party makes to its own
//! copy. The folder arithmetic on a row's values lives in
//! [`crate::domain::message`].

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::message::{
    Folder, Message, MessageBody, MessageId, MessageLabel, MessageSubject,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn send(
    db: &Database,
    sender: &UserId,
    recipient: &UserId,
    subject: MessageSubject,
    body: MessageBody,
    label: Option<MessageLabel>,
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
    db: &Database,
    user: &UserId,
    folder: Folder,
    read: Option<bool>,
    limit: Option<i64>,
    offset: i64,
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
    db: &Database,
    id: &MessageId,
    user: &UserId,
) -> Result<Option<Message>, AppError> {
    let message: Option<Message> = db.select(id.record()).await?;
    Ok(message.filter(|message| {
        (&message.sender == user || &message.recipient == user)
            && message.folder_of(user) != Folder::Deleted
    }))
}

/// Flip the recipient's read flag. Field-scoped write: the sender may be
/// filing their side concurrently, and a whole-row save would clobber it.
pub async fn set_read(db: &Database, message: Message, read: bool) -> Result<Message, AppError> {
    let mut result = db
        .query("UPDATE $id SET read = $read RETURN AFTER")
        .bind(("id", message.id.record()))
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
/// the same race reason as [`set_read`].
///
/// Filing away remembers the folder being left, so `inbox → archive →
/// trash` restores to `archive` and that back to `inbox`. Two folders are
/// never stamped: the trash (`trash → archive` would leave the copy
/// pointing back at the discard pile — it falls back to the home folder)
/// and the target itself (a no-op move keeps the memory it had). Moving
/// back to a home folder clears it.
pub async fn move_to(
    db: &Database,
    message: Message,
    user: &UserId,
    folder: Folder,
) -> Result<Message, AppError> {
    let current = message.folder_of(user);
    let (field, origin_field) = if message.is_sender(user) {
        ("sender_folder", "sender_origin")
    } else {
        ("recipient_folder", "recipient_origin")
    };
    let origin = match folder {
        _ if !folder.is_filed() => None,
        _ if current == folder => message.origin_of(user),
        _ if current.is_restore_target() => Some(current),
        _ => None,
    };
    let mut result = db
        .query(format!(
            "UPDATE $id SET {field} = $folder, {origin_field} = $origin RETURN AFTER"
        ))
        .bind(("id", message.id.record()))
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
pub async fn delete_for(db: &Database, message: Message, user: &UserId) -> Result<(), AppError> {
    let (field, origin_field) = if message.is_sender(user) {
        ("sender_folder", "sender_origin")
    } else {
        ("recipient_folder", "recipient_origin")
    };
    let mut result = db
        .query(format!(
            "UPDATE $id SET {field} = $deleted, {origin_field} = NONE \
             WHERE {field} = $trash RETURN AFTER"
        ))
        .bind(("id", message.id.record()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `next_ulid` hazard: an inbox is documented newest-first and sorts
    /// `id DESC`, so messages sent inside one millisecond must come back in
    /// exact reverse write order, not at random (src/domain/monotonic_id.rs).
    /// Asserts the *stored* order, not the mint order.
    #[tokio::test]
    async fn inbox_is_newest_first_within_a_millisecond() {
        let db = crate::database::init_mem().await.unwrap();
        let sender = UserId::from_key("a");
        let recipient = UserId::from_key("b");
        let mut sent = Vec::new();
        for i in 0..25 {
            let message = send(
                &db,
                &sender,
                &recipient,
                MessageSubject::try_new(&format!("s{i}")).unwrap(),
                MessageBody::try_new("").unwrap(),
                None,
            )
            .await
            .unwrap();
            sent.push(message.get_id().key().to_string());
        }

        let (listed, total) = list_folder(&db, &recipient, Folder::Inbox, None, None, 0)
            .await
            .unwrap();
        assert_eq!(total, 25);
        sent.reverse();
        let read_back: Vec<String> = listed
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        assert_eq!(read_back, sent);
    }
}
