//! The person tables against the **control** database: the global account,
//! its school memberships, and its login sessions keyed by cookie token. A
//! person row exists once no matter how many school databases hold an
//! `app_user` copy; the credential newtypes are [`crate::domain::user`]'s and
//! the register-side workflow is [`crate::service::person`].

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::person::{Membership, Person, PersonId, PersonSession};
use crate::domain::session::{SessionId, SessionToken};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, Username};
use crate::error::AppError;
use crate::tenant::{SchoolId, SchoolStatus, Slug};

/// The raw account insert behind [`crate::service::person::create_or_load`].
/// The unique index on `username` is the whole availability check; the loser
/// of a race gets the same `Conflict` a sequential duplicate would.
pub async fn create(db: &Database, person: Person) -> Result<(), AppError> {
    match sqlx::query!(
        "INSERT INTO person (id, username, password_hash, created_at) VALUES ($1, $2, $3, $4)",
        person.id.uuid(),
        person.username.as_str(),
        person.password_hash.as_str(),
        Timestamp::now().as_millis(),
    )
    .execute(db)
    .await
    {
        Ok(_) => Ok(()),
        Err(err) if crate::database::unique_violation(&err) == Some("person_username") => {
            Err(AppError::Conflict("username already taken"))
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &PersonId) -> Result<Option<Person>, AppError> {
    let person = sqlx::query_as!(
        Person,
        r#"SELECT id AS "id: PersonId",
                  username AS "username: Username",
                  password_hash AS "password_hash: PasswordHash"
           FROM person WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(person)
}

/// The login lookup. Usernames are stored trimmed; callers trim the same way
/// so a padded attempt matches the canonical name.
pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<Person>, AppError> {
    let person = sqlx::query_as!(
        Person,
        r#"SELECT id AS "id: PersonId",
                  username AS "username: Username",
                  password_hash AS "password_hash: PasswordHash"
           FROM person WHERE username = $1"#,
        username.trim()
    )
    .fetch_optional(db)
    .await?;
    Ok(person)
}

/// Every school the person belongs to, slug order, with the registry's name
/// and status joined in. Suspended schools are part of the answer — the
/// caller decides what to offer.
pub async fn memberships(db: &Database, person: &PersonId) -> Result<Vec<Membership>, AppError> {
    let rows = sqlx::query_as!(
        Membership,
        r#"SELECT s.slug AS "slug: Slug",
                  s.name,
                  s.status AS "status: SchoolStatus"
           FROM person_school ps
           JOIN school s ON s.id = ps.school
           WHERE ps.person = $1
           ORDER BY s.slug"#,
        person.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The person's membership in exactly one school — the `POST /auth/school`
/// gate. A slug the person does not hold is the same `None` a malformed one
/// is: no existence signal either way.
pub async fn membership_of(
    db: &Database,
    person: &PersonId,
    slug: &Slug,
) -> Result<Option<Membership>, AppError> {
    let row = sqlx::query_as!(
        Membership,
        r#"SELECT s.slug AS "slug: Slug",
                  s.name,
                  s.status AS "status: SchoolStatus"
           FROM person_school ps
           JOIN school s ON s.id = ps.school
           WHERE ps.person = $1 AND s.slug = $2"#,
        person.uuid(),
        slug.as_str()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Attach a person to a school. Idempotent by the primary key: a racing or
/// repeated join is a no-op, not an error. The membership carries the
/// school's uuid, resolved from the slug in the same statement — so a school
/// deleted mid-flight joins nothing instead of erroring, which is the honest
/// outcome (there is nothing left to join).
pub async fn add_membership(db: &Database, person: &PersonId, slug: &Slug) -> Result<(), AppError> {
    sqlx::query!(
        "INSERT INTO person_school (person, school, created_at)
         SELECT $1, s.id, $3 FROM school s WHERE s.slug = $2
         ON CONFLICT DO NOTHING",
        person.uuid(),
        slug.as_str(),
        Timestamp::now().as_millis()
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Drop every membership pointing at a school — the control-plane half of
/// [`crate::tenant::Tenants::drop`], inside the transaction that also drops the
/// entitlements and the registry row, so `ON DELETE NO ACTION` never refuses
/// the registry delete and a failure leaves none of the three behind. Persons
/// themselves survive: a person is a global account, not the school's.
pub async fn delete_memberships_by_school(
    exe: impl sqlx::PgExecutor<'_>,
    school: &SchoolId,
) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM person_school WHERE school = $1", school.uuid())
        .execute(exe)
        .await?;
    Ok(())
}

/// Mint and store one person login session (the `person.` cookie).
pub async fn create_session(db: &Database, person: &PersonId) -> Result<PersonSession, AppError> {
    let id = SessionId::generate();
    let token = SessionToken::generate()?;
    let expires_at = Timestamp::in_days(SESSION_DURATION_DAYS);
    let session = sqlx::query_as!(
        PersonSession,
        r#"INSERT INTO person_session (id, person, token, created_at, expires_at)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING person AS "person: PersonId",
                     token AS "token: SessionToken",
                     expires_at AS "expires_at: Timestamp""#,
        id.uuid(),
        person.uuid(),
        token.as_str(),
        Timestamp::now().as_millis(),
        expires_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(session)
}

pub async fn find_session_by_token(
    db: &Database,
    token: &str,
) -> Result<Option<PersonSession>, AppError> {
    let session = sqlx::query_as!(
        PersonSession,
        r#"SELECT person AS "person: PersonId",
                  token AS "token: SessionToken",
                  expires_at AS "expires_at: Timestamp"
           FROM person_session WHERE token = $1"#,
        token
    )
    .fetch_optional(db)
    .await?;
    Ok(session)
}

pub async fn delete_session_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM person_session WHERE token = $1", token)
        .execute(db)
        .await?;
    Ok(())
}

/// Revoke every person-login session for this account — the other half of a
/// password reset. School sessions are deleted separately against the school
/// database; leaving these alive would let a `person.<token>` cookie mint a
/// new school session with the old credential.
pub async fn delete_sessions_by_person(db: &Database, person: &PersonId) -> Result<(), AppError> {
    sqlx::query!(
        "DELETE FROM person_session WHERE person = $1",
        person.uuid()
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Move the person credential — the one login reads. The school-side row
/// carries no password at all, so this is the whole credential move.
pub async fn set_password_hash(
    db: &Database,
    username: &Username,
    password_hash: &PasswordHash,
) -> Result<(), AppError> {
    sqlx::query!(
        "UPDATE person SET password_hash = $2 WHERE username = $1",
        username.as_str(),
        password_hash.as_str()
    )
    .execute(db)
    .await?;
    Ok(())
}
