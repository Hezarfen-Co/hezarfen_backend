//! One paged `SELECT` — the window and the `total` that goes with it, both
//! done by the database.
//!
//! WHY: `?limit=10` used to run an unbounded `SELECT *`, hand the whole table
//! to the web layer and slice ten rows off it. The scan (and the decode of
//! every row) grew with the table no matter how small the page was. Here the
//! window is `LIMIT/OFFSET` in SQL, and `total` is a `count(*)` over the
//! *same* `WHERE` as a second statement, so a page can never disagree with
//! the count that pages it.
//!
//! ```ignore
//! PagedList::new("note WHERE user_id = $1", "ORDER BY id DESC")
//!     .bind(owner.0)
//!     .run::<Note>(limit, offset, db)
//!     .await
//! ```
//!
//! Paging stays opt-in: `limit = None, offset = 0` is the unpaged read, and
//! it runs exactly one statement (no count — the rows in hand *are* the
//! total), so internal callers that just want the list pay nothing.
//!
//! Runtime-checked SQL by design: `from_where` is assembled per call site,
//! which is this module's named exemption from the compile-time `query!`
//! rule. Placeholders are positional (`$1, $2, …`) in [`.bind`] order, and
//! the window's own placeholders continue the numbering. The count is a plain
//! `count(*)`, which always answers one `NOT NULL` row — no planner-shape
//! decoding to preserve.
//!
//! The dynamic parts of every statement here are table/column names (in-crate
//! constants and literals, never client text) and the `$n` numbers this
//! builder prints itself; every value binds — which is the audit the
//! [`AssertSqlSafe`] wraps stand on.
//!
//! Only for lists the database can express whole. A handler that filters rows
//! in Rust after the read (visibility, audience resolution, a schedule
//! window) must keep slicing in the web layer: a DB `LIMIT` in front of a
//! Rust filter silently returns a short page.

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Arguments, AssertSqlSafe, FromRow};
use uuid::Uuid;

use crate::database::Database;
use crate::error::AppError;

/// One bound parameter of a runtime-checked builder's statement, in
/// `$1, $2, …` order.
///
/// [`PagedList`] and [`crate::db::field_update::FieldUpdate`] are the named
/// exemptions from the compile-time `query!` rule: their SQL is assembled at
/// run time, so their parameters ride this closed enum — owned values, no
/// borrow to tie a builder's lifetime to, [`Clone`] because one builder run
/// can need the same `WHERE` bound twice (page, then count). A caller that
/// needs a type the enum lacks adds a variant here; sqlx's borrow-tied
/// dynamic binding does not survive being stored in a builder.
#[derive(Clone, Debug)]
pub(crate) enum Param {
    I64(i64),
    Text(String),
    Uuid(Uuid),
    /// A nullable column value: `None` binds SQL `NULL` with the uuid type
    /// still named, so `IS NOT DISTINCT FROM $n` compares an absent link
    /// truthfully.
    OptUuid(Option<Uuid>),
    /// Nullable TEXT column clear/set — same NULL-with-type rationale as
    /// OptUuid.
    OptText(Option<String>),
    /// Nullable BIGINT column clear/set.
    OptI64(Option<i64>),
}

impl Param {
    /// Push onto runtime-checked arguments. sqlx defers encode failures to
    /// execution, so there is nothing to check here.
    pub(crate) fn add_to(self, args: &mut PgArguments) {
        let _ = match self {
            Param::I64(value) => args.add(value),
            Param::Text(value) => args.add(value),
            Param::Uuid(value) => args.add(value),
            Param::OptUuid(value) => args.add(value),
            Param::OptText(value) => args.add(value),
            Param::OptI64(value) => args.add(value),
        };
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Uuid> for Param {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

impl From<Option<Uuid>> for Param {
    fn from(value: Option<Uuid>) -> Self {
        Self::OptUuid(value)
    }
}

pub(crate) struct PagedList {
    /// Everything after `FROM`, e.g. `note WHERE user_id = $1`. Placeholders
    /// are positional in [`PagedList::bind`] order; shared verbatim by the
    /// page and the count.
    from_where: String,
    /// The full `ORDER BY ...`, applied to the page only.
    order: &'static str,
    binds: Vec<Param>,
}

impl PagedList {
    /// `from_where` is a whole-table read (`"term"`) or a filtered one
    /// (`"note WHERE user_id = $1"`).
    pub(crate) fn new(from_where: impl Into<String>, order: &'static str) -> Self {
        Self {
            from_where: from_where.into(),
            order,
            binds: Vec::new(),
        }
    }

    /// Bind the next positional parameter: the first call fills `$1`, the
    /// second `$2`, … — `from_where` must carry the placeholders in the same
    /// order.
    #[must_use]
    pub(crate) fn bind<T: Into<Param>>(mut self, value: T) -> Self {
        self.binds.push(value.into());
        self
    }

    /// The page statement and, when the window can hide rows, the count
    /// statement. Split out so a test can read the SQL: the whole point of
    /// this type is that `LIMIT/OFFSET` reach the database instead of a
    /// `Vec` slice.
    fn statements(&self, limit: Option<i64>, offset: i64) -> (String, Option<String>) {
        let mut next = self.binds.len();
        let mut sql = format!("SELECT * FROM {} {}", self.from_where, self.order);
        if limit.is_some() {
            next += 1;
            sql.push_str(&format!(" LIMIT ${next}"));
        }
        // A window that can hide rows needs the count; an unpaged read from
        // row zero already holds every row, so it stays a single statement.
        let counted = limit.is_some() || offset > 0;
        if counted {
            next += 1;
            sql.push_str(&format!(" OFFSET ${next}"));
        }
        let count = counted.then(|| format!("SELECT count(*) FROM {}", self.from_where));
        (sql, count)
    }

    /// The page plus the full row count. `limit = None` runs from `offset` to
    /// the end, matching the unpaged envelope.
    pub(crate) async fn run<T>(
        self,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<T>, i64), AppError>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        let (page_sql, count_sql) = self.statements(limit, offset);
        let mut args = PgArguments::default();
        for bind in self.binds.iter().cloned() {
            bind.add_to(&mut args);
        }
        if let Some(limit) = limit {
            Param::from(limit).add_to(&mut args);
        }
        if count_sql.is_some() {
            Param::from(offset).add_to(&mut args);
        }
        let rows: Vec<T> =
            sqlx::query_as_with(AssertSqlSafe(page_sql), args).fetch_all(db).await?;
        let total = match count_sql {
            Some(count_sql) => {
                let mut args = PgArguments::default();
                for bind in self.binds {
                    bind.add_to(&mut args);
                }
                let scalar: i64 =
                    sqlx::query_scalar_with(AssertSqlSafe(count_sql), args).fetch_one(db).await?;
                scalar
            }
            None => rows.len() as i64,
        };
        Ok((rows, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> PagedList {
        PagedList::new("note WHERE user_id = $1", "ORDER BY id DESC").bind(Uuid::nil())
    }

    /// The window has to be SQL — a `Vec` slice would leave the scan
    /// unbounded, which is the whole reason this type exists.
    #[test]
    fn a_paged_read_windows_and_counts_in_sql() {
        let (sql, count) = list().statements(Some(10), 20);
        assert!(sql.contains("ORDER BY id DESC LIMIT $2 OFFSET $3"));
        assert_eq!(
            count.as_deref(),
            Some("SELECT count(*) FROM note WHERE user_id = $1")
        );
    }

    /// A whole-table read counts the bare table.
    #[test]
    fn an_unfiltered_list_counts_the_bare_table() {
        let (sql, count) =
            PagedList::new("term", "ORDER BY starts_at DESC").statements(Some(5), 0);
        assert!(sql.contains("LIMIT $1 OFFSET $2"));
        assert_eq!(count.as_deref(), Some("SELECT count(*) FROM term"));
    }

    /// Unpaged from row zero: the rows in hand are the total, so no count
    /// runs.
    #[test]
    fn an_unpaged_read_stays_one_statement() {
        let (sql, count) = list().statements(None, 0);
        assert_eq!(
            sql,
            "SELECT * FROM note WHERE user_id = $1 ORDER BY id DESC"
        );
        assert_eq!(count, None);
        // An offset without a limit still hides rows, so it needs the count.
        let (sql, count) = list().statements(None, 3);
        assert!(sql.contains("OFFSET $2"));
        assert!(!sql.contains("LIMIT"));
        assert!(count.is_some());
    }

    /// The window's placeholders continue the caller's numbering, in emission
    /// order.
    #[test]
    fn window_placeholders_continue_the_where_numbering() {
        let (sql, count) =
            PagedList::new("note WHERE user_id = $1 AND archived_at IS NULL", "ORDER BY id")
                .bind(Uuid::nil())
                .statements(Some(2), 4);
        assert!(sql.contains("LIMIT $2 OFFSET $3"));
        assert_eq!(
            count.as_deref(),
            Some("SELECT count(*) FROM note WHERE user_id = $1 AND archived_at IS NULL")
        );
    }
}
