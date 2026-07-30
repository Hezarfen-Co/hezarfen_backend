//! One `UPDATE $id SET ... RETURN AFTER` built from *only* the fields a PATCH
//! actually carried.
//!
//! WHY: a handler reads the row, then writes. Filling a field the request
//! omitted from that snapshot re-sends a value the client never gave, so a
//! concurrent PATCH of the *other* field that landed in between is silently
//! reverted — the same lost update a whole-row `.content(self)` save causes,
//! just spelled field by field. Scoping the `SET` is not enough; the *values* must come
//! from the request, so an absent field is never written at all.
//!
//! Callers therefore take `Option<T>` per field (`None` = absent, keep) and
//! hand them straight here:
//!
//! ```ignore
//! FieldUpdate::new(self.id.record())
//!     .set("title", title)
//!     .set("content", content)
//!     .run::<Note>(db)
//!     .await
//! ```
//!
//! Clearing a nullable column is `Some(None)` on a `Option<Option<T>>` field
//! — pass it as `Some(value_or_none)` so `NONE` is written explicitly.

use surrealdb::types::{RecordId, SurrealValue, Value};

use crate::database::{Database, write_with_retry};
use crate::error::AppError;

pub struct FieldUpdate {
    id: RecordId,
    /// `field = $field` fragments, in the order they were set.
    assignments: Vec<String>,
    bindings: Vec<(String, Value)>,
    /// `(low, high, refusal)` for [`FieldUpdate::ordered`].
    ordered: Option<(&'static str, &'static str, AppError)>,
    /// `(condition, refusal)` for [`FieldUpdate::guard`].
    guard: Option<(&'static str, AppError)>,
}

impl FieldUpdate {
    pub fn new(id: RecordId) -> Self {
        Self {
            id,
            assignments: Vec::new(),
            bindings: Vec::new(),
            ordered: None,
            guard: None,
        }
    }

    /// Write `field` only when the request carried it. The bind variable is
    /// named after the field, so `id` is the one name a caller must not use.
    #[must_use]
    pub fn set<T: SurrealValue>(mut self, field: &'static str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.assignments.push(format!("{field} = ${field}"));
            self.bindings.push((field.to_string(), value.into_value()));
        }
        self
    }

    /// Refuse the write unless `low <= high` still holds on the *merged* row —
    /// the new value for an end this PATCH carries, the stored column for one it
    /// omits. Emitted as a `WHERE` on the same `UPDATE`, so the comparison the
    /// handler made against its snapshot is re-made by the database at write
    /// time, against the row as it is *then*: two PATCHes each moving one end,
    /// each fine on its own, cannot commit an inverted range between them. That
    /// is what an in-process `Mutex` used to buy, without a window between the
    /// snapshot and the write for a concurrent request to slip into.
    ///
    /// Call it after the `.set()`s of both fields. A PATCH that moves neither
    /// end emits no guard at all (nothing to race), matching the lock window it
    /// replaces — so a pre-existing inverted row stays editable field by field.
    /// `NONE` on either side passes, exactly like
    /// [`crate::web::check_time_range`] skipping an absent end.
    #[must_use]
    pub fn ordered(mut self, low: &'static str, high: &'static str, refused: AppError) -> Self {
        self.ordered = Some((low, high, refused));
        self
    }

    /// Refuse the write unless `condition` — a predicate on this same row —
    /// still holds at write time. The [`FieldUpdate::ordered`] guard for a
    /// precondition that is not about a range: a handler that read "nothing
    /// references this yet" re-asks the database at the instant it writes, so a
    /// reference landing in between refuses the edit instead of being edited
    /// out from under. `condition` is always an in-crate SQL literal, never
    /// user input.
    ///
    /// A request that carries no field at all emits no `UPDATE`, so it is not
    /// refused: it writes nothing, and reading the row back is a truthful
    /// answer to a PATCH that asked for no change.
    #[must_use]
    pub fn guard(mut self, condition: &'static str, refused: AppError) -> Self {
        self.guard = Some((condition, refused));
        self
    }

    /// Run the update and return the stored row. An empty request writes
    /// nothing at all and reads the row back unchanged.
    pub async fn run<T: SurrealValue>(mut self, db: &Database) -> Result<T, AppError> {
        if self.assignments.is_empty() {
            let row: Option<T> = db.select(self.id).await?;
            return row.ok_or(AppError::NotFound);
        }
        let (ordered, mut refused) = match self.ordered.take() {
            // Both ends stored: name the bound variable for an end this request
            // set, the column for one it left alone.
            Some((low, high, err)) if self.is_set(low) || self.is_set(high) => {
                let side = |field: &str, set| {
                    if set {
                        format!("${field}")
                    } else {
                        field.into()
                    }
                };
                let low = side(low, self.is_set(low));
                let high = side(high, self.is_set(high));
                (
                    Some(format!(
                        "({low} = NONE OR {high} = NONE OR {low} <= {high})"
                    )),
                    Some(err),
                )
            }
            _ => (None, None),
        };
        // Both guards land on the one `UPDATE`, ANDed. No caller sets both, so
        // the refusal is whichever one is there.
        let (extra, extra_refused) = match self.guard.take() {
            Some((condition, err)) => (Some(condition.to_string()), Some(err)),
            None => (None, None),
        };
        refused = refused.or(extra_refused);
        let conditions: Vec<String> = ordered.into_iter().chain(extra).collect();
        let guard = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sets = self.assignments.join(", ");
        let sql = format!("UPDATE $id SET {sets}{guard} RETURN AFTER");
        self.bindings.push(("id".into(), self.id.into_value()));
        // Retried on a write conflict: a guarded PATCH contends on the very row
        // its guard reads, and the loser wrote nothing, so re-sending it is the
        // recovery — see [`write_with_retry`].
        let rows: Vec<T> = write_with_retry(db, &sql, &self.bindings).await?;
        // No row back means the guard bit (or, in the window after the handler's
        // read, the row was deleted — the guard cannot tell the two apart, and
        // reports the refusal it was given).
        rows.into_iter()
            .next()
            .ok_or_else(|| refused.take().unwrap_or(AppError::NotFound))
    }

    fn is_set(&self, field: &str) -> bool {
        self.bindings.iter().any(|(name, _)| name == field)
    }
}
