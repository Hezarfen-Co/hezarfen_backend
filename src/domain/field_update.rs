//! One `UPDATE $id SET ... RETURN AFTER` built from *only* the fields a PATCH
//! actually carried.
//!
//! WHY: a handler reads the row, then writes. Filling a field the request
//! omitted from that snapshot re-sends a value the client never gave, so a
//! concurrent PATCH of the *other* field that landed in between is silently
//! reverted — the same lost update the whole-row-save gate bans, just spelled
//! field by field. Scoping the `SET` is not enough; the *values* must come
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
}

impl FieldUpdate {
    pub fn new(id: RecordId) -> Self {
        Self {
            id,
            assignments: Vec::new(),
            bindings: Vec::new(),
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

    /// Run the update and return the stored row. An empty request writes
    /// nothing at all and reads the row back unchanged.
    pub async fn run<T: SurrealValue>(self, db: &Database) -> Result<T, AppError> {
        if self.assignments.is_empty() {
            let row: Option<T> = db.select(self.id).await?;
            return row.ok_or(AppError::NotFound);
        }
        let sets = self.assignments.join(", ");
        let mut query = db
            .query(format!("UPDATE $id SET {sets} RETURN AFTER"))
            .bind(("id", self.id));
        for (name, value) in self.bindings {
            query = query.bind((name, value));
        }
        let mut result = query.await?.check()?;
        result
            .take::<Vec<T>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}
