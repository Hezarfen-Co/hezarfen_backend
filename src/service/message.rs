//! Message workflows. Every persistence step here is a single atomic
//! record write ([`crate::db::message`]) whose rule rides in its own
//! `WHERE` clause, so there is no multi-call lock to hold and no policy of
//! its own — the handler validates the DTO, gates on the row it just read,
//! and sequences these calls. They live in the service so the web layer
//! never names the db layer.

use crate::database::Database;
use crate::db::message;
use crate::domain::message::{
    Folder, Message, MessageBody, MessageId, MessageLabel, MessageSubject,
};
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
    message::send(db, sender, recipient, subject, body, label).await
}

pub async fn list_folder(
    db: &Database,
    user: &UserId,
    folder: Folder,
    read: Option<bool>,
    q: Option<&str>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Message>, i64), AppError> {
    message::list_folder(db, user, folder, read, q, limit, offset).await
}

pub async fn read_for(
    db: &Database,
    id: &MessageId,
    user: &UserId,
) -> Result<Option<Message>, AppError> {
    message::read_for(db, id, user).await
}

pub async fn set_read(
    db: &Database,
    message_row: Message,
    read: bool,
) -> Result<Message, AppError> {
    message::set_read(db, message_row, read).await
}

pub async fn move_to(
    db: &Database,
    message_row: Message,
    user: &UserId,
    folder: Folder,
) -> Result<Message, AppError> {
    message::move_to(db, message_row, user, folder).await
}

pub async fn delete_for(
    db: &Database,
    message_row: Message,
    user: &UserId,
) -> Result<(), AppError> {
    message::delete_for(db, message_row, user).await
}
