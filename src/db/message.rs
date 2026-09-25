//! The `message` table: the send-side create, the folder listings, the
//! party-scoped read, and the field-scoped writes each party makes to its own
//! copy. The folder arithmetic on a row's values lives in
//! [`crate::domain::message`].

use crate::database::Database;
use crate::db::page::{PagedList, Param};
use crate::domain::message::{
    Folder, Message, MessageBody, MessageId, MessageLabel, MessageSubject,
};
use crate::domain::text_fold::{search_fold, search_fold_sql};
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
///
/// `q` is an optional free-text needle over subject and body, folded
/// case- and diacritic-insensitively on both sides ([`search_fold`] /
/// [`search_fold_sql`]); a blank needle searches nothing.
pub async fn list_folder(
    db: &Database,
    user: &UserId,
    folder: Folder,
    read: Option<bool>,
    q: Option<&str>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Message>, i64), AppError> {
    // A blank needle searches nothing, exactly like an absent one.
    let needle = q.map(|q| search_fold(q.trim())).filter(|q| !q.is_empty());
    let home_arm = folder == Folder::Inbox;
    let mut from = match folder {
        Folder::Sent => "message WHERE sender = $1 AND sender_folder = 'sent'".to_string(),
        Folder::Archive => "message WHERE (recipient = $1 AND recipient_folder = 'archive') \
             OR (sender = $1 AND sender_folder = 'archive')"
            .to_string(),
        Folder::Trash => "message WHERE (recipient = $1 AND recipient_folder = 'trash') \
             OR (sender = $1 AND sender_folder = 'trash')"
            .to_string(),
        _ => "message WHERE recipient = $1 AND recipient_folder = $2".to_string(),
    };
    // The `read` filter binds after whichever placeholders the folder shape
    // spent (`$1` is always the user; the home-folder arm spends `$2`).
    let mut next = if home_arm { 3 } else { 2 };
    if read.is_some() {
        from.push_str(&format!(" AND read = ${next}"));
        next += 1;
    }
    // The needle rides the same fold on both sides (`position`, not `LIKE`,
    // keeps `%` and `_` literal). Subject or body — this query joins no
    // counterparty, so names are not searched.
    if needle.is_some() {
        from.push_str(&format!(
            " AND (position(${next} in {}) > 0 OR position(${next} in {}) > 0)",
            search_fold_sql("subject"),
            search_fold_sql("body"),
        ));
    }
    let mut builder = PagedList::new(from, "ORDER BY id DESC").bind(user.uuid());
    if home_arm {
        builder = builder.bind(folder.as_str().to_string());
    }
    if let Some(read) = read {
        builder = builder.bind(Param::Bool(read));
    }
    if let Some(needle) = needle {
        builder = builder.bind(needle);
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

        let (listed, total) =
            list_folder(&db, &recipient, Folder::Inbox, None, None, None, 0)
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

    /// `q` folds case and Turkish diacritics on both sides, matches subject
    /// or body, composes with `read`, and pages after the filter (`total`
    /// counts matches, not rows). The folder predicate still scopes the
    /// search: a stranger's folders stay empty no matter what the needle
    /// matches in someone else's mail.
    #[tokio::test]
    async fn q_searches_subject_and_body_and_composes_with_read() {
        let (db, _leases) = crate::database::init_test_db().await;
        let sender = crate::db::class_member::tests::fixture_user(&db, "q-gonderen").await;
        let recipient = crate::db::class_member::tests::fixture_user(&db, "q-alici").await;
        let stranger = crate::db::class_member::tests::fixture_user(&db, "q-yabanci").await;

        let subject_hit = send(
            &db,
            &sender,
            &recipient,
            MessageSubject::try_new("Matematik Etüt").unwrap(),
            MessageBody::try_new("saat üçte").unwrap(),
            None,
        )
        .await
        .unwrap();
        let body_hit = send(
            &db,
            &sender,
            &recipient,
            MessageSubject::try_new("duyuru").unwrap(),
            MessageBody::try_new("İSTANBUL gezisi etüt sonrası").unwrap(),
            None,
        )
        .await
        .unwrap();
        send(
            &db,
            &sender,
            &recipient,
            MessageSubject::try_new("toplantı").unwrap(),
            MessageBody::try_new("alakasız").unwrap(),
            None,
        )
        .await
        .unwrap();
        let subject_key = subject_hit.get_id().key().to_string();
        let body_key = body_hit.get_id().key().to_string();

        // Absent and blank needles both see the whole folder.
        let (_, total) =
            list_folder(&db, &recipient, Folder::Inbox, None, None, None, 0)
                .await
                .unwrap();
        assert_eq!(total, 3);
        let (_, total) =
            list_folder(&db, &recipient, Folder::Inbox, None, Some("   "), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 3);

        // Subject hit through the fold: `etut` finds `Etüt`.
        let (hits, total) =
            list_folder(&db, &recipient, Folder::Inbox, None, Some("etut"), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 2);
        let keys: Vec<String> = hits
            .iter()
            .map(|message| message.get_id().key().to_string())
            .collect();
        assert!(keys.contains(&subject_key));
        assert!(keys.contains(&body_key));

        // Body hit, case-insensitive in both directions.
        let (hits, total) = list_folder(
            &db,
            &recipient,
            Folder::Inbox,
            None,
            Some("İsTaNbUl"),
            None,
            0,
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].get_id().key().to_string(), body_key);

        // A needle matching nothing: an empty page, not an error.
        let (hits, total) = list_folder(
            &db,
            &recipient,
            Folder::Inbox,
            None,
            Some("yok boyle bir sey"),
            None,
            0,
        )
        .await
        .unwrap();
        assert_eq!(total, 0);
        assert!(hits.is_empty());

        // The window runs after the filter: `total` counts matches.
        let (hits, total) = list_folder(
            &db,
            &recipient,
            Folder::Inbox,
            None,
            Some("etut"),
            Some(1),
            0,
        )
        .await
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 1);

        // Composes with `read`: one match read, the other not.
        set_read(&db, subject_hit, true).await.unwrap();
        let (_, total) = list_folder(
            &db,
            &recipient,
            Folder::Inbox,
            Some(true),
            Some("etut"),
            None,
            0,
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        let (hits, total) = list_folder(
            &db,
            &recipient,
            Folder::Inbox,
            Some(false),
            Some("etut"),
            None,
            0,
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].get_id().key().to_string(), body_key);

        // The folder predicate still scopes the search: the stranger's own
        // folders are empty even though the needle matches other users' mail.
        let (hits, total) =
            list_folder(&db, &stranger, Folder::Inbox, None, Some("etut"), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 0);
        assert!(hits.is_empty());
        let (hits, total) =
            list_folder(&db, &stranger, Folder::Sent, None, Some("etut"), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 0);
        assert!(hits.is_empty());
    }
}
