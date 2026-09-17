use std::collections::{HashMap, HashSet};

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::message::{
    Folder, Message, MessageBody, MessageId, MessageLabel, MessageSubject,
};
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service::message;
use crate::state::AppState;

use super::dto::Role;
use super::{CurrentUser, Page, PageParams, PersonRef};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(send_message, list))
        .routes(routes!(update_message, delete_message))
}

#[derive(Deserialize, ToSchema)]
struct SendMessage {
    /// The receiving user's id (find people via `GET /users/search`).
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    recipient_id: String,
    #[schema(example = "About today's study session", max_length = 200)]
    subject: String,
    #[schema(example = "I may be 10 minutes late.", max_length = 10000)]
    body: Option<String>,
    /// Optional free-text tag the UI shows as a badge ("Study", "Exam", …).
    /// Blank counts as absent.
    #[schema(example = "Etüt", max_length = 50)]
    label: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateMessage {
    /// Mark read (`true`) or unread (`false`). Recipient only.
    read: Option<bool>,
    /// Move the caller's copy: a recipient may file into `inbox`, `archive`,
    /// or `trash`; a sender into `sent`, `archive`, or `trash`. Restoring is
    /// moving back to the copy's `previous_folder`.
    #[schema(example = "archive")]
    folder: Option<String>,
}

#[derive(Deserialize, IntoParams)]
struct ListFilter {
    /// Which folder to list: `inbox` (default), `sent`, `archive`, or `trash`.
    #[param(example = "inbox")]
    folder: Option<String>,
    /// Narrow by the read flag: `false` = unread only (so
    /// `?folder=inbox&read=false&limit=1` makes `total` the unread badge),
    /// `true` = read only. Omit for both.
    read: Option<bool>,
}

/// A message as one party sees it. `folder` is the *caller's* copy's folder;
/// `read` is the recipient's flag (senders see it as a read receipt). The
/// `*_role` fields are `null` only when that account has since been deleted.
#[derive(Serialize, ToSchema)]
struct MessageResponse {
    id: String,
    sender: PersonRef,
    sender_role: Option<Role>,
    recipient: PersonRef,
    recipient_role: Option<Role>,
    subject: String,
    body: String,
    /// The sender's badge tag, if they set one.
    #[schema(example = "Etüt")]
    label: Option<String>,
    /// When it was sent, UTC unix-milliseconds (server-stamped).
    sent_at: i64,
    read: bool,
    #[schema(example = "inbox")]
    folder: String,
    /// Where the caller's copy sat before it was filed into `archive`/`trash`
    /// — `PATCH` `folder` back to this to restore it. `null` whenever the copy
    /// is not filed away, and on copies filed before this was recorded; both
    /// mean "restore to `inbox`" (`sent` for the sender's copy).
    #[schema(example = "inbox")]
    previous_folder: Option<String>,
}

/// `PersonRef` plus role, keyed by user id — messages show *who* wrote and
/// their role badge, so the plain `person_map` isn't enough.
type People = HashMap<String, (PersonRef, Role)>;

fn people_of<'a>(users: impl IntoIterator<Item = &'a User>) -> People {
    users
        .into_iter()
        .map(|user| {
            (
                user.get_id().key().to_string(),
                (PersonRef::new(user), user.get_role().into()),
            )
        })
        .collect()
}

/// Load the two ends of every listed message into a `People` map, one query.
async fn load_people(messages: &[Message], db: &Database) -> Result<People, AppError> {
    let mut seen = HashSet::new();
    let ids: Vec<UserId> = messages
        .iter()
        .flat_map(|message| [*message.get_sender(), *message.get_recipient()])
        .filter(|id| seen.insert(id.key().to_string()))
        .collect();
    let users = crate::service::user::list_by_ids(db, &ids).await?;
    Ok(people_of(&users))
}

impl MessageResponse {
    fn new(message: &Message, caller: &UserId, people: &People) -> Self {
        let resolve = |id: &UserId| match people.get(id.key().as_str()) {
            Some((person, role)) => (person.clone(), Some(*role)),
            None => (
                PersonRef {
                    id: id.key().to_string(),
                    username: id.key().to_string(),
                    display_name: None,
                },
                None,
            ),
        };
        let (sender, sender_role) = resolve(message.get_sender());
        let (recipient, recipient_role) = resolve(message.get_recipient());
        Self {
            id: message.get_id().key().to_string(),
            sender,
            sender_role,
            recipient,
            recipient_role,
            subject: message.get_subject().as_str().to_string(),
            body: message.get_body().as_str().to_string(),
            label: message.get_label().map(|label| label.as_str().to_string()),
            sent_at: message.get_sent_at().as_millis(),
            read: message.is_read(),
            folder: message.folder_of(caller).as_str().to_string(),
            previous_folder: message
                .origin_of(caller)
                .map(|folder| folder.as_str().to_string()),
        }
    }
}

/// Send a message to another user. Messaging is upward only below staff: a
/// student or parent may write to a teacher, manager or admin (parents
/// included — messaging is the one place a parent acts), never to another
/// student or parent; staff write to anyone. Messaging yourself is refused.
/// The send is server-stamped and lands in the recipient's inbox unread.
#[utoipa::path(
    post,
    path = "/",
    tag = "messages",
    security(("session_cookie" = [])),
    request_body = SendMessage,
    responses(
        (status = 201, description = "Message sent", body = MessageResponse),
        (status = 400, description = "Invalid subject or body, or the recipient is yourself", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "A student or parent wrote to someone who is not staff", body = ErrorResponse),
        (status = 404, description = "No such recipient", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn send_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<SendMessage>,
) -> Result<(StatusCode, Json<MessageResponse>), AppError> {
    let subject = MessageSubject::try_new(&req.subject)?;
    let body = MessageBody::try_new(&req.body.unwrap_or_default())?;
    // A blank label is "no label", not an error — the field is a badge, and
    // an empty badge means the sender just didn't pick one.
    let label = match req.label.as_deref().map(str::trim).unwrap_or_default() {
        "" => None,
        value => Some(MessageLabel::try_new(value)?),
    };
    if req.recipient_id == user.get_id().key() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "recipient_id",
            reason: "cannot message yourself",
        }));
    }
    let recipient = crate::service::user::read(&st.db, &UserId::from_key(&req.recipient_id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !user.get_role().may_message(recipient.get_role()) {
        return Err(AppError::Forbidden(
            "students and parents may only message staff (teacher or higher)",
        ));
    }

    let message = message::send(
        &st.db,
        user.get_id(),
        recipient.get_id(),
        subject,
        body,
        label,
    )
    .await?;
    let people = people_of([&user, &recipient]);
    let response = MessageResponse::new(&message, user.get_id(), &people);
    Ok((StatusCode::CREATED, Json(response)))
}

/// List one of the caller's folders, newest first: `inbox` (default),
/// `sent`, `archive`, or `trash` (`?folder=`). `?read=` narrows by the read
/// flag; `total` counts the filtered folder, so
/// `?folder=inbox&read=false&limit=1` is a cheap unread badge. Paged via
/// `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/",
    tag = "messages",
    security(("session_cookie" = [])),
    params(ListFilter, PageParams),
    responses(
        (status = 200, description = "A page of the folder's messages", body = Page<MessageResponse>),
        (status = 400, description = "Unknown folder, or invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(filter): Query<ListFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MessageResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let folder = match filter.folder {
        Some(ref folder) => Folder::try_new(folder)?,
        None => Folder::Inbox,
    };

    let (messages, total) =
        message::list_folder(&st.db, user.get_id(), folder, filter.read, limit, offset).await?;
    let people = load_people(&messages, &st.db).await?;
    let items = messages
        .iter()
        .map(|message| MessageResponse::new(message, user.get_id(), &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Update the caller's view of a message: flip the read flag (recipient
/// only) and/or move the caller's copy between folders. Each side files
/// independently — archiving or trashing never touches the other party's
/// copy. Filing into `archive`/`trash` records the folder left behind as the
/// copy's `previous_folder`, so restoring is a move back to that value
/// (`inbox`, or `sent` for the sender's copy, when it is `null`). Omitted
/// fields change nothing.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "messages",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Message id")),
    request_body = UpdateMessage,
    responses(
        (status = 200, description = "The updated message", body = MessageResponse),
        (status = 400, description = "A folder this side of the message cannot move to", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only the recipient can change the read flag", body = ErrorResponse),
        (status = 404, description = "Not found (or not a party to it)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateMessage>,
) -> Result<Json<MessageResponse>, AppError> {
    let mut message = message::read_for(&st.db, &MessageId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;

    // Validate the whole patch before touching the row: a rejected folder
    // must not leave a half-applied read flag behind.
    if req.read.is_some() && message.is_sender(user.get_id()) {
        return Err(AppError::Forbidden(
            "only the recipient can change the read flag",
        ));
    }
    let folder = match req.folder {
        Some(ref folder) => {
            let folder = Folder::try_new(folder)?;
            if !Folder::allowed_for(message.is_sender(user.get_id())).contains(&folder) {
                return Err(AppError::Validation(ValidationError::Invalid {
                    field: "folder",
                    reason: "not a folder this side of the message can move to",
                }));
            }
            Some(folder)
        }
        None => None,
    };
    if let Some(read) = req.read {
        message = message::set_read(&st.db, message, read).await?;
    }
    if let Some(folder) = folder {
        message = message::move_to(&st.db, message, user.get_id(), folder).await?;
    }

    let people = load_people(std::slice::from_ref(&message), &st.db).await?;
    Ok(Json(MessageResponse::new(&message, user.get_id(), &people)))
}

/// Permanently delete the caller's copy — allowed only from the trash
/// (`PATCH` it to `folder: "trash"` first). The other party's copy lives on;
/// the row disappears for good once both sides have deleted theirs.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "messages",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Message id")),
    responses(
        (status = 204, description = "Deleted for the caller"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not a party to it)", body = ErrorResponse),
        (status = 409, description = "The message is not in the caller's trash", body = ErrorResponse),
    ),
)]
async fn delete_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let message = message::read_for(&st.db, &MessageId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    message::delete_for(&st.db, message, user.get_id()).await?;
    Ok(StatusCode::NO_CONTENT)
}
