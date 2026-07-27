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

use crate::database::Database;
use crate::error::AppError;

pub struct FieldUpdate {
    id: RecordId,
    /// `field = $field` fragments, in the order they were set.
    assignments: Vec<String>,
    bindings: Vec<(String, Value)>,
    /// `(low, high, refusal)` for [`FieldUpdate::ordered`].
    ordered: Option<(&'static str, &'static str, AppError)>,
}

impl FieldUpdate {
    pub fn new(id: RecordId) -> Self {
        Self {
            id,
            assignments: Vec::new(),
            bindings: Vec::new(),
            ordered: None,
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
    /// is what an in-process `Mutex` used to buy, and this holds across replicas.
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

    /// Run the update and return the stored row. An empty request writes
    /// nothing at all and reads the row back unchanged.
    pub async fn run<T: SurrealValue>(mut self, db: &Database) -> Result<T, AppError> {
        if self.assignments.is_empty() {
            let row: Option<T> = db.select(self.id).await?;
            return row.ok_or(AppError::NotFound);
        }
        let (guard, mut refused) = match self.ordered.take() {
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
                    format!(" WHERE ({low} = NONE OR {high} = NONE OR {low} <= {high})"),
                    Some(err),
                )
            }
            _ => (String::new(), None),
        };
        let sets = self.assignments.join(", ");
        let mut query = db
            .query(format!("UPDATE $id SET {sets}{guard} RETURN AFTER"))
            .bind(("id", self.id));
        for (name, value) in self.bindings {
            query = query.bind((name, value));
        }
        let mut result = query.await?.check()?;
        // No row back means the guard bit (or, in the window after the handler's
        // read, the row was deleted — the guard cannot tell the two apart, and
        // reports the refusal it was given).
        result
            .take::<Vec<T>>(0)?
            .into_iter()
            .next()
            .ok_or_else(|| refused.take().unwrap_or(AppError::NotFound))
    }

    fn is_set(&self, field: &str) -> bool {
        self.bindings.iter().any(|(name, _)| name == field)
    }
}
