//! The `message` table: the send-side create, the folder listings, the
//! party-scoped read, and the field-scoped writes each party makes to its own
//! copy. The folder arithmetic on a row's values lives in
//! [`crate::domain::message`].

use crate::database::Database;
use crate::db::page::{PagedList, Param};
use crate::domain::message::{
    Folder, Message, MessageBody, MessageId, MessageLabel, MessageSubject,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

pub async fn send(
    db: &Database,
    sender: &UserId,
    recipient: &UserId,
    subject: MessageSubject,
    body: MessageBody,
    label: Option<MessageLabel>,
) -> Result<Message, AppError> {
    let message = query_as!(
        Message,
        "INSERT INTO message (id, sender, recipient, subject, body, label, sent_at, read, \
                             sender_folder, recipient_folder, sender_origin, recipient_origin)
         VALUES ($1, $2, $3, $4, $5, $6, $7, false, 'sent', 'inbox', NULL, NULL)
         RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
        MessageId::generate().uuid(),
        sender.uuid(),
        recipient.uuid(),
        subject.as_str(),
        body.as_str(),
        label.map(|l| l.as_str().to_string()),
        Timestamp::now().as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(message)
}

/// One folder view for `user`, newest first. `inbox` is recipient-side,
/// `sent` is sender-side, and `archive`/`trash` are each the union of both
/// sides' filed copies (either party may archive or trash their own copy).
/// `read` narrows to that flag state (`false` on the inbox is the unread
/// view; on `sent`, receipts pending).
///
/// The folder predicates are the four closed shapes the domain's folder
/// vocabulary admits, spelled as one `WHERE` each — the same shapes the old
/// dynamic string assembled, minus the interpolation. They ride the
/// [`PagedList`] builder (the paged-list exemption), so the page and its
/// count always see the same predicate.
pub async fn list_folder(
    db: &Database,
    user: &UserId,
    folder: Folder,
    read: Option<bool>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Message>, i64), AppError> {
    let home_arm = folder == Folder::Inbox;
    let mut from = match folder {
        Folder::Sent => "message WHERE sender = $1 AND sender_folder = 'sent'".to_string(),
        Folder::Archive => {
            "message WHERE (recipient = $1 AND recipient_folder = 'archive') \
             OR (sender = $1 AND sender_folder = 'archive')"
                .to_string()
        }
        Folder::Trash => {
            "message WHERE (recipient = $1 AND recipient_folder = 'trash') \
             OR (sender = $1 AND sender_folder = 'trash')"
                .to_string()
        }
        _ => "message WHERE recipient = $1 AND recipient_folder = $2".to_string(),
    };
    // The `read` filter binds after whichever placeholders the folder shape
    // spent (`$1` is always the user; the home-folder arm spends `$2`).
    if read.is_some() {
        from.push_str(&format!(" AND read = ${}", if home_arm { 3 } else { 2 }));
    }
    let mut builder = PagedList::new(from, "ORDER BY id DESC").bind(user.uuid());
    if home_arm {
        builder = builder.bind(folder.as_str().to_string());
    }
    if let Some(read) = read {
        builder = builder.bind(Param::Bool(read));
    }
    builder.run(limit, offset, db).await
}

/// Read a message only if `user` is its sender or recipient and their
/// side hasn't been permanently deleted.
pub async fn read_for(
    db: &Database,
    id: &MessageId,
    user: &UserId,
) -> Result<Option<Message>, AppError> {
    let message = query_as!(
        Message,
        "SELECT id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                sent_at AS \"sent_at: Timestamp\", read, \
                sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\" \
         FROM message WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(message.filter(|message| {
        (&message.sender == user || &message.recipient == user)
            && message.folder_of(user) != Folder::Deleted
    }))
}

/// Flip the recipient's read flag. Field-scoped write: the sender may be
/// filing their side concurrently, and a whole-row save would clobber it.
pub async fn set_read(db: &Database, message: Message, read: bool) -> Result<Message, AppError> {
    let updated = query_as!(
        Message,
        "UPDATE message SET read = $1 WHERE id = $2 \
         RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
        read,
        message.get_id().uuid(),
    )
    .fetch_optional(db)
    .await?;
    updated.ok_or(AppError::NotFound)
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
    let origin = match folder {
        _ if !folder.is_filed() => None,
        _ if current == folder => message.origin_of(user),
        _ if current.is_restore_target() => Some(current),
        _ => None,
    };
    let origin = origin.map(|origin| origin.as_str().to_string());
    // One static statement per side: the field pair written is the caller's
    // own, so the sender can never clobber the recipient's filing flags.
    let updated = if message.is_sender(user) {
        query_as!(
            Message,
            "UPDATE message SET sender_folder = $1, sender_origin = $2 WHERE id = $3 \
             RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
            folder.as_str(),
            origin,
            message.get_id().uuid(),
        )
        .fetch_optional(db)
        .await?
    } else {
        query_as!(
            Message,
            "UPDATE message SET recipient_folder = $1, recipient_origin = $2 WHERE id = $3 \
             RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
            folder.as_str(),
            origin,
            message.get_id().uuid(),
        )
        .fetch_optional(db)
        .await?
    };
    updated.ok_or(AppError::NotFound)
}

/// Permanently drop `user`'s side — only if that side currently sits in
/// the trash; the gate rides in the `UPDATE`'s `WHERE` so a concurrent
/// move back to the inbox can't slip past a check-then-act window. The
/// row itself is removed once both sides are gone — judged on the
/// post-write row, so two concurrent deletes can't leak an all-deleted
/// row.
pub async fn delete_for(db: &Database, message: Message, user: &UserId) -> Result<(), AppError> {
    let after = if message.is_sender(user) {
        query_as!(
            Message,
            "UPDATE message SET sender_folder = 'deleted', sender_origin = NULL \
             WHERE id = $1 AND sender_folder = 'trash' \
             RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
            message.get_id().uuid(),
        )
        .fetch_optional(db)
        .await?
    } else {
        query_as!(
            Message,
            "UPDATE message SET recipient_folder = 'deleted', recipient_origin = NULL \
             WHERE id = $1 AND recipient_folder = 'trash' \
             RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
            message.get_id().uuid(),
        )
        .fetch_optional(db)
        .await?
    };
    let Some(after) = after else {
        return Err(AppError::Conflict(
            "only messages in the trash can be permanently deleted",
        ));
    };
    if after.sender_folder == Folder::Deleted && after.recipient_folder == Folder::Deleted {
        query_as!(
            Message,
            "DELETE FROM message WHERE id = $1 \
             RETURNING id AS \"id: MessageId\", sender AS \"sender: UserId\", recipient AS \"recipient: UserId\", \
                   subject AS \"subject: MessageSubject\", body AS \"body: MessageBody\", label AS \"label: MessageLabel\", \
                   sent_at AS \"sent_at: Timestamp\", read, \
                   sender_folder AS \"sender_folder: Folder\", recipient_folder AS \"recipient_folder: Folder\", \
                   sender_origin AS \"sender_origin: Folder\", recipient_origin AS \"recipient_origin: Folder\"",
            after.get_id().uuid(),
        )
        .fetch_optional(db)
        .await?;
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
        let (db, _leases) = crate::database::init_test_db().await;
        let sender = crate::db::class_member::tests::fixture_user(&db, "gonderen").await;
        let recipient = crate::db::class_member::tests::fixture_user(&db, "alici").await;
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
