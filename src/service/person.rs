//! Person workflows on the control database: the create-or-load behind the
//! register and school-create paths, and the credential mirroring that keeps
//! a person and their school `app_user` copies in step. The queries live in
//! [`crate::db::person`]; login itself stays in the web layer, which is the
//! one place that sees both the control handle and the school handles.

use crate::database::Database;
use crate::db::person;
use crate::domain::person::{Membership, Person, PersonId, PersonSession};
use crate::domain::user::{PasswordHash, Username};
use crate::error::AppError;
use crate::tenant::Slug;

/// Create the person, or hand back the one already standing under
/// `username`. The incoming hash is stored only on the create path — an
/// existing person's stored credential is never rewritten here, because the
/// caller has not yet proven they know it. The register and school-create
/// callers verify against the returned row before joining.
pub async fn create_or_load(
    control: &Database,
    username: Username,
    password_hash: PasswordHash,
) -> Result<Person, AppError> {
    let created = person::create(
        control,
        Person {
            id: PersonId::generate(),
            username: username.clone(),
            password_hash,
        },
    )
    .await;
    match created {
        Ok(()) | Err(AppError::Conflict(_)) => person::find_by_username(control, username.as_str())
            .await?
            .ok_or_else(|| AppError::Internal("the person vanished as it was written".into())),
        Err(err) => Err(err),
    }
}

/// Attach a person to a school. Idempotent — see [`person::add_membership`].
pub async fn link_school(
    control: &Database,
    person: &PersonId,
    slug: &Slug,
) -> Result<(), AppError> {
    person::add_membership(control, person, slug).await
}

pub async fn read(control: &Database, id: &PersonId) -> Result<Option<Person>, AppError> {
    person::read(control, id).await
}

/// The login lookup. Usernames are stored trimmed; callers trim the same way
/// so a padded attempt matches the canonical name.
pub async fn find_by_username(
    control: &Database,
    username: &str,
) -> Result<Option<Person>, AppError> {
    person::find_by_username(control, username).await
}

pub async fn memberships(
    control: &Database,
    person: &PersonId,
) -> Result<Vec<Membership>, AppError> {
    person::memberships(control, person).await
}

pub async fn membership_of(
    control: &Database,
    person: &PersonId,
    slug: &Slug,
) -> Result<Option<Membership>, AppError> {
    person::membership_of(control, person, slug).await
}

pub async fn create_session(
    control: &Database,
    person: &PersonId,
) -> Result<PersonSession, AppError> {
    person::create_session(control, person).await
}

pub async fn find_session_by_token(
    control: &Database,
    token: &str,
) -> Result<Option<PersonSession>, AppError> {
    person::find_session_by_token(control, token).await
}

pub async fn delete_session_by_token(control: &Database, token: &str) -> Result<(), AppError> {
    person::delete_session_by_token(control, token).await
}

pub async fn delete_sessions_by_person(
    control: &Database,
    person: &PersonId,
) -> Result<(), AppError> {
    person::delete_sessions_by_person(control, person).await
}

/// Move the credential — the one login reads. The school-side row carries no
/// password at all, so this is the whole credential move.
pub async fn set_password_hash(
    control: &Database,
    username: &Username,
    password_hash: &PasswordHash,
) -> Result<(), AppError> {
    person::set_password_hash(control, username, password_hash).await
}
