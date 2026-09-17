//! Term workflows: the archived-refusal rule every write against past
//! structure pays, the id resolver a request-supplied term passes through,
//! the row-level write gate, the delete's linked-refusal mapping, and the
//! archive that freezes every report card inside the term. The queries live in
//! [`crate::db::term`].

use crate::database::Database;
use crate::db::term;
use crate::domain::academic_year::AcademicYearId;
use crate::domain::term::{Term, TermId, TermName, archived_error};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};

/// Mint a term inside an academic year. The year is claimed in the same
/// statement as the row (see [`term::create`]): past years take no new
/// structure, which is why the web layer resolves the id through
/// [`crate::service::academic_year::resolve`] first.
pub async fn create(
    db: &Database,
    name: TermName,
    year: AcademicYearId,
    starts_at: Timestamp,
    ends_at: Timestamp,
) -> Result<Term, AppError> {
    term::create(db, name, year, starts_at, ends_at).await
}

/// The term row, for callers that only inspect it — the web layer's gates
/// read through here.
pub async fn read(db: &Database, id: &TermId) -> Result<Option<Term>, AppError> {
    term::read(db, id).await
}

pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Term>, i64), AppError> {
    term::list_all(db, limit, offset).await
}

/// Turn an optional request-supplied term id into a validated reference —
/// `None` stays `None`, an unknown id is a `400` naming the field, and an
/// *archived* one is a `409 term_archived`. That last refusal lives here
/// rather than in each handler because this is the single spot every
/// request-supplied term passes through (an exam names one): past years take
/// no new structure.
pub async fn resolve(db: &Database, id: Option<&str>) -> Result<Option<TermId>, AppError> {
    let Some(id) = id else {
        return Ok(None);
    };
    let resolved = term::read(db, &TermId::from_key(id))
        .await?
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "term_id",
            reason: "term does not exist",
        }))?;
    if resolved.is_archived() {
        return Err(archived_error());
    }
    Ok(Some(*resolved.get_id()))
}

/// Refuse when the named term is archived. A missing row is `Ok(())`: a
/// dangling link is not this guard's error, and the caller that cares
/// already answers it (see [`crate::domain::term::gone_error`]).
pub async fn require_open(db: &Database, id: &TermId) -> Result<(), AppError> {
    match term::read(db, id).await? {
        Some(term) if term.is_archived() => Err(archived_error()),
        _ => Ok(()),
    }
}

/// Refuse when this term row is archived — the read-only rule for past
/// structure, judged against a row the caller already read through [`read`];
/// a missing row is the caller's 404, not this guard's answer.
pub fn require_writable(t: &Term) -> Result<(), AppError> {
    if t.is_archived() {
        return Err(archived_error());
    }
    Ok(())
}

/// Update an open term; an archived one is refused — past years are
/// read-only.
pub async fn update(
    db: &Database,
    target: Term,
    name: Option<TermName>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<Term, AppError> {
    term::update(db, target, name, starts_at, ends_at).await
}

/// Delete the term while nothing links it. The linked-refusal is the rule's
/// own answer, so the message the web layer used to pick is coded here; a
/// row that is already gone is the store's `404` ([`term::delete`]).
pub async fn delete(db: &Database, target: Term) -> Result<(), AppError> {
    if term::delete(db, target).await? {
        Ok(())
    } else {
        Err(AppError::Conflict(
            "exams are still linked to this dönem, or a karne was frozen for it — \
             nothing unlinks them for you",
        ))
    }
}

/// Archive a term; idempotent — an already-archived term answers with the
/// stamp it already had.
///
/// The archive freezes every student's report card for the term first
/// ([`crate::service::karne::freeze`]): from the moment the term is closed
/// the report is the record the school issued, and a later correction to a
/// mark must not silently rewrite what a family holds. The freeze runs *before*
/// the stamp so a failure leaves nothing archived-but-unfrozen — an archived
/// term whose snapshot is missing would go on serving live computations, and
/// the store's delete guard (which refuses a term a report card was frozen
/// for) would not see it either. A term that is already archived is left
/// alone: a second archive must not re-freeze over a record that was already
/// issued.
pub async fn archive(db: &Database, target: Term) -> Result<Term, AppError> {
    if !target.is_archived() {
        crate::service::karne::freeze(db, target.get_id()).await?;
    }
    term::archive(db, target).await
}

/// Re-open an archived term; idempotent the same way.
pub async fn unarchive(db: &Database, target: Term) -> Result<Term, AppError> {
    term::unarchive(db, target).await
}
