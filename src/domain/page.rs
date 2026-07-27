//! One paged `SELECT` — the window and the `total` that goes with it, both
//! done by the database.
//!
//! WHY: `?limit=10` used to run an unbounded `SELECT *`, hand the whole table
//! to the web layer and slice ten rows off it. The scan (and the decode of
//! every row) grew with the table no matter how small the page was. Here the
//! window is `LIMIT/START` in SQL, and `total` is a `count()` over the *same*
//! `WHERE`, so a page can never disagree with the count that pages it.
//!
//! ```ignore
//! PagedList::new("note WHERE user = $usr", "ORDER BY id DESC")
//!     .bind("usr", owner.record())
//!     .run::<Note>(limit, offset, db)
//!     .await
//! ```
//!
//! Paging stays opt-in: `limit = None, offset = 0` is the unpaged read, and it
//! emits exactly one statement (no count — the rows in hand *are* the total),
//! so internal callers that just want the list pay nothing.
//!
//! Only for lists the database can express whole. A handler that filters rows
//! in Rust after the read (visibility, audience resolution, a schedule window)
//! must keep slicing in the web layer: a DB `LIMIT` in front of a Rust filter
//! silently returns a short page.

use surrealdb::types::{SurrealValue, Value};

use crate::database::Database;
use crate::error::AppError;

pub struct PagedList {
    /// Everything after `FROM`, e.g. `note WHERE user = $usr`. Shared verbatim
    /// by the page and the count.
    from_where: String,
    /// The full `ORDER BY ...`, applied to the page only.
    order: &'static str,
    bindings: Vec<(String, Value)>,
}

impl PagedList {
    /// `from_where` is a whole-table read (`"term"`) or a filtered one
    /// (`"note WHERE user = $usr"`).
    pub fn new(from_where: impl Into<String>, order: &'static str) -> Self {
        let from_where = from_where.into();
        Self {
            // A *bare* `SELECT VALUE count() FROM t GROUP ALL` comes back as an
            // object rather than the projected int ("Expected int, got object")
            // — the whole-table read takes a path that drops the `VALUE`. Any
            // `WHERE`, even a trivial one, keeps it an int, so an unfiltered
            // list gets one. Cheaper than special-casing the take.
            from_where: match from_where.contains(" WHERE ") {
                true => from_where,
                false => format!("{from_where} WHERE true"),
            },
            order,
            bindings: Vec::new(),
        }
    }

    /// Bind one variable of the `WHERE`. `limit` and `offset` are the two names
    /// a caller must not use.
    #[must_use]
    pub fn bind<T: SurrealValue>(mut self, name: &'static str, value: T) -> Self {
        self.bindings.push((name.to_string(), value.into_value()));
        self
    }

    /// The statements to run, and whether the second one (the count) is among
    /// them. Split out so a test can read the SQL: the whole point of this type
    /// is that `LIMIT`/`START` reach the database instead of a `Vec` slice.
    fn statements(&self, limit: Option<i64>, offset: i64) -> (String, bool) {
        let from_where = &self.from_where;
        let window = match limit {
            Some(_) => "LIMIT $limit START $offset",
            None => "START $offset",
        };
        let mut sql = format!("SELECT * FROM {from_where} {} {window};", self.order);
        // A window that can hide rows needs the count; an unpaged read from row
        // zero already holds every row, so it stays a single statement.
        let counted = limit.is_some() || offset > 0;
        if counted {
            sql.push_str(&format!(
                "SELECT VALUE count() FROM {from_where} GROUP ALL;"
            ));
        }
        (sql, counted)
    }

    /// The page plus the full row count. `limit = None` runs from `offset` to
    /// the end, matching the unpaged envelope.
    pub async fn run<T: SurrealValue>(
        self,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<T>, i64), AppError> {
        let (sql, counted) = self.statements(limit, offset);
        let mut query = db
            .query(sql)
            .bind(("offset", offset))
            .bind(("limit", limit.unwrap_or(0)));
        for (name, value) in self.bindings {
            query = query.bind((name, value));
        }
        let mut result = query.await?.check()?;
        let rows = result.take::<Vec<T>>(0)?;
        let total = if counted {
            // `GROUP ALL` yields no row at all when nothing matched.
            result.take::<Vec<i64>>(1)?.first().copied().unwrap_or(0)
        } else {
            rows.len() as i64
        };
        Ok((rows, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database;
    use crate::domain::note::{Note, NoteContent, NoteTitle};
    use crate::domain::user::UserId;
    use ulid::Ulid;

    fn list() -> PagedList {
        PagedList::new("note WHERE user = $usr", "ORDER BY id DESC")
    }

    /// The window has to be SQL — a `Vec` slice would leave the scan unbounded,
    /// which is the whole reason this type exists.
    #[test]
    fn a_paged_read_windows_and_counts_in_sql() {
        let (sql, counted) = list().statements(Some(10), 20);
        assert!(sql.contains("ORDER BY id DESC LIMIT $limit START $offset;"));
        assert!(sql.contains("SELECT VALUE count() FROM note WHERE user = $usr GROUP ALL;"));
        assert!(counted);
    }

    /// A whole-table read still gets a `WHERE`, or its count comes back as an
    /// object instead of an int.
    #[test]
    fn an_unfiltered_list_still_counts_as_an_int() {
        let (sql, _) = PagedList::new("term", "ORDER BY starts_at DESC").statements(Some(5), 0);
        assert!(sql.contains("SELECT VALUE count() FROM term WHERE true GROUP ALL;"));
    }

    /// Unpaged from row zero: the rows in hand are the total, so no count runs.
    #[test]
    fn an_unpaged_read_stays_one_statement() {
        let (sql, counted) = list().statements(None, 0);
        assert!(sql.ends_with("ORDER BY id DESC START $offset;"));
        assert!(!sql.contains("count()"));
        assert!(!counted);
        // An offset without a limit still hides rows, so it needs the count.
        assert!(list().statements(None, 3).1);
    }

    #[tokio::test]
    async fn the_window_walks_the_rows_and_total_stays_the_whole_list() {
        let db = database::init_mem().await.unwrap();
        let user = UserId::from_key(&Ulid::new().to_string());
        for i in 0..5 {
            Note::create(
                &user,
                NoteTitle::try_new(&format!("n{i}")).unwrap(),
                NoteContent::try_new("x").unwrap(),
                &db,
            )
            .await
            .unwrap();
        }

        // Every page is `limit` long and `total` ignores the window.
        let (page, total) = Note::list_for(&user, Some(2), 0, &db).await.unwrap();
        assert_eq!((page.len(), total), (2, 5));
        let (tail, total) = Note::list_for(&user, Some(2), 4, &db).await.unwrap();
        assert_eq!((tail.len(), total), (1, 5));
        // Offset past the end is an empty page, not an error.
        assert_eq!(
            Note::list_for(&user, Some(2), 9, &db)
                .await
                .unwrap()
                .0
                .len(),
            0
        );
        // Unpaged, and unpaged-from-an-offset: the count still covers everything.
        let (all, total) = Note::list_for(&user, None, 0, &db).await.unwrap();
        assert_eq!((all.len(), total), (5, 5));
        let (rest, total) = Note::list_for(&user, None, 3, &db).await.unwrap();
        assert_eq!((rest.len(), total), (2, 5));

        // The pages are consecutive slices of the unpaged list — page
        // boundaries match the order the ORDER BY already produced.
        let keys: Vec<&str> = all.iter().map(|note| note.get_id().key()).collect();
        assert_eq!(
            page.iter().map(|n| n.get_id().key()).collect::<Vec<_>>(),
            keys[..2]
        );
        assert_eq!(
            tail.iter().map(|n| n.get_id().key()).collect::<Vec<_>>(),
            keys[4..]
        );
    }
}
