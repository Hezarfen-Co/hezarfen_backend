//! The README's `## Endpoints` table is generated, not written by hand.
//!
//! A hand-maintained route table drifts the moment a handler changes: the
//! `.claude/kb-sync.sh` hook only catches a *missing* path, never a stale Auth
//! cell nor a row for a route the code deleted. So this test renders the table
//! from the served `/api-docs/openapi.json` and asserts the marker block in
//! README.md matches it. Regenerate after any route change:
//!
//! ```text
//! UPDATE_DOCS=1 cargo test --test doc_sync
//! ```
//!
//! Two columns come straight out of the spec (Path/Method, and Description =
//! the handler's doc comment, which utoipa publishes as the operation summary).
//! The **Auth** column cannot: OpenAPI has no field for "minimum role", and the
//! `403` descriptions only sometimes spell it out. So the floor is read from the
//! two places that actually enforce it — the declared `403` when it names a role
//! (`"Requires manager role or higher"`), otherwise the `Require*` extractor in
//! the handler's own signature. Every operation must resolve to exactly one
//! floor or this test fails naming it, so the column can never go quietly wrong.

mod common;

use std::collections::HashMap;

use serde_json::Value;

const BEGIN: &str = "<!-- BEGIN GENERATED: endpoint-table -->";
const END: &str = "<!-- END GENERATED -->";

fn repo() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Routes no OpenAPI document can carry, in the shape the table wants them.
/// `/` is a plain `axum::routing::get` (no `routes!` macro, so no operation);
/// the two doc endpoints are mounted by `SwaggerUi`, outside the
/// `OpenApiRouter`; the two `…/ws` rooms are WebSocket upgrades, which the spec
/// itself calls out as "Not in this spec (WebSocket)" in their tag text.
/// Everything else is derived — do not grow this list to paper over a missing
/// `#[utoipa::path]`.
const STATIC_ROWS: [(&str, &str, &str, &str); 5] = [
    ("GET", "/", "no", "Same as `/health`"),
    ("GET", "/api-docs/openapi.json", "no", "Raw OpenAPI 3 spec"),
    (
        "GET",
        "/boards/{id}/ws",
        "student",
        "**WebSocket** board room: a `join` replays the current epoch, every accepted stroke fans out to the other participants (see \"Collaborative whiteboard\")",
    ),
    (
        "GET",
        "/exams/{id}/attempt/ws",
        "student",
        "**WebSocket** exam room (students only): state ticks, autosave, finish; entering clears `left_at`, leaving stamps it (see \"Taking an exam\")",
    ),
    ("GET", "/swagger", "no", "Interactive API docs (Swagger UI)"),
];

/// Table order within one path. The repo uses `GET`/`POST`/`PATCH`/`DELETE`
/// only (CLAUDE.md: no `PUT`); anything else sorts last.
fn method_rank(m: &str) -> usize {
    ["GET", "POST", "PATCH", "DELETE"]
        .iter()
        .position(|x| *x == m)
        .unwrap_or(usize::MAX)
}

/// `s[open..]` starts at a `(`; returns the index of its matching `)`.
fn matching_paren(s: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (i, byte) in s.as_bytes().iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced `(` in source at byte {open}");
}

/// The value of a `key = "…"` entry inside a `#[utoipa::path(…)]` attribute.
fn attr_str<'a>(attr: &'a str, key: &str) -> Option<&'a str> {
    let start = attr.find(&format!("{key} = \""))? + key.len() + 4;
    let len = attr[start..].find('"')?;
    Some(&attr[start..start + len])
}

/// The role floor a handler's argument list enforces. `CurrentUser` and the
/// bare extractors gate on a session only, which the README calls `student`
/// ("any logged-in user").
fn extractor_floor(params: &str) -> &'static str {
    for (ty, role) in [
        ("RequireAdmin", "admin"),
        ("RequireManager", "manager"),
        ("RequireTeacher", "teacher"),
    ] {
        if params.contains(ty) {
            return role;
        }
    }
    "student"
}

/// Every `#[utoipa::path]`-annotated handler in `src/`, keyed by the four
/// things the spec also publishes: method, tag, operation id (= the fn name)
/// and the route path *relative to its nest prefix*. That key needs no
/// knowledge of how `lib.rs` nests routers, which is why it is used instead of
/// reconstructing full paths.
type HandlerKey = (String, String, String, String);

/// Every `src/**/*.rs` as (path relative to the repo, contents).
fn src_files() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![repo().join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read source file");
            let name = path
                .strip_prefix(repo())
                .unwrap_or(&path)
                .display()
                .to_string();
            out.push((name, src));
        }
    }
    out
}

fn handlers() -> HashMap<HandlerKey, Vec<&'static str>> {
    let mut out: HashMap<HandlerKey, Vec<&'static str>> = HashMap::new();
    for (_, src) in src_files() {
        let mut at = 0;
        while let Some(hit) = src[at..].find("#[utoipa::path(") {
            let open = at + hit + "#[utoipa::path".len();
            let close = matching_paren(&src, open);
            let attr = &src[open + 1..close];
            at = close + 1;

            // `)]`, then the item: `pub(crate) async fn name(params)`.
            let rest = src[close + 1..].trim_start_matches(']').trim_start();
            let rest = rest.trim_start_matches("pub").trim_start();
            // `pub(crate)` / `pub(super)` visibility qualifier.
            let rest = if rest.starts_with('(') {
                rest[matching_paren(rest, 0) + 1..].trim_start()
            } else {
                rest
            };
            let rest = rest.trim_start_matches("async").trim_start();
            let Some(rest) = rest.strip_prefix("fn ") else {
                continue;
            };
            let Some(paren) = rest.find('(') else {
                continue;
            };
            let name = rest[..paren].trim().to_string();
            let params = &rest[paren + 1..matching_paren(rest, paren)];

            let method = attr
                .trim()
                .split(',')
                .next()
                .expect("attribute has a method")
                .trim()
                .to_uppercase();
            let key = (
                method,
                attr_str(attr, "tag").unwrap_or("").to_string(),
                name,
                attr_str(attr, "path")
                    .unwrap_or("")
                    .trim_end_matches('/')
                    .to_string(),
            );
            out.entry(key).or_default().push(extractor_floor(params));
        }
    }
    out
}

/// The minimum role for one operation. `no` when the operation carries no
/// security requirement at all; otherwise the role its `403` declares, and
/// failing that the one its handler's extractor enforces.
fn auth_of(
    path: &str,
    method: &str,
    op: &Value,
    handlers: &HashMap<HandlerKey, Vec<&str>>,
) -> String {
    let has_security = op["security"].as_array().is_some_and(|s| !s.is_empty());
    if !has_security {
        return "no".into();
    }
    let forbidden = op["responses"]["403"]["description"].as_str().unwrap_or("");
    if let Some(rest) = forbidden.split_once("Requires ").map(|(_, r)| r) {
        let mut words = rest.split_whitespace();
        let first = words.next().unwrap_or("");
        let role = if first == "the" {
            words.next().unwrap_or("")
        } else {
            first
        };
        let role = role.trim_matches(|c: char| !c.is_ascii_alphabetic());
        if ["parent", "student", "teacher", "manager", "admin"].contains(&role) {
            return role.to_string();
        }
    }

    let tag = op["tags"][0].as_str().unwrap_or("");
    let operation_id = op["operationId"].as_str().unwrap_or("");
    let mut floors: Vec<&str> = handlers
        .iter()
        .filter(|((m, t, n, rel), _)| {
            m == method && t == tag && n == operation_id && (rel.is_empty() || path.ends_with(rel))
        })
        .flat_map(|(_, floors)| floors.iter().copied())
        .collect();
    floors.sort_unstable();
    floors.dedup();
    assert_eq!(
        floors.len(),
        1,
        "cannot pin the role floor of `{method} {path}` (tag {tag}, operationId \
         {operation_id}): matched {floors:?} in src/. Either its `#[utoipa::path]` \
         `403` should name the role (\"Requires <role> role or higher\"), or the \
         handler lookup above needs to learn a new shape."
    );
    floors[0].to_string()
}

/// The Description cell: the operation summary, which utoipa lifts from the
/// first paragraph of the handler's doc comment. Folded to one line, with `|`
/// escaped so a cell cannot break the table. A handler with no doc comment
/// falls back to its success response's description — still spec text, never
/// invented — and both cases mean "write a doc comment".
fn description_of(op: &Value) -> String {
    let summary = op["summary"].as_str().unwrap_or("").trim();
    let text = if summary.is_empty() {
        op["responses"]
            .as_object()
            .and_then(|r| {
                r.iter()
                    .filter(|(k, _)| k.starts_with('2'))
                    .min_by_key(|(k, _)| k.to_string())
                    .and_then(|(_, v)| v["description"].as_str())
            })
            .unwrap_or("")
    } else {
        summary
    };
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Render the whole table (header included) from the served spec.
fn render(spec: &Value) -> String {
    let handlers = handlers();
    let mut rows: Vec<(String, String, String, String)> = STATIC_ROWS
        .iter()
        .map(|(m, p, a, d)| (m.to_string(), p.to_string(), a.to_string(), d.to_string()))
        .collect();

    let paths = spec["paths"]
        .as_object()
        .expect("the served spec has a paths object");
    for (path, item) in paths {
        for (method, op) in item.as_object().expect("path item is an object") {
            // A path item also carries non-verb keys (`parameters`, `summary`);
            // an operation is the thing with `responses`.
            if !op.get("responses").is_some_and(Value::is_object) {
                continue;
            }
            let method = method.to_uppercase();
            let auth = auth_of(path, &method, op, &handlers);
            rows.push((method, path.clone(), auth, description_of(op)));
        }
    }
    rows.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(method_rank(&a.0).cmp(&method_rank(&b.0)))
    });

    // Pad the fixed-width columns to their widest cell, as the hand-written
    // table did; Description is last, so it never needs padding.
    let path_w = rows.iter().map(|r| r.1.len() + 2).max().unwrap_or(4).max(4);
    let auth_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(4).max(4);
    let mut out = format!(
        "| Method | {:pw$} | {:aw$} | Description |\n|--------|{}|{}|-------------|\n",
        "Path",
        "Auth",
        "-".repeat(path_w + 2),
        "-".repeat(auth_w + 2),
        pw = path_w,
        aw = auth_w,
    );
    for (method, path, auth, desc) in &rows {
        out.push_str(&format!(
            "| {:6} | {:pw$} | {:aw$} | {desc} |\n",
            method,
            format!("`{path}`"),
            auth,
            pw = path_w,
            aw = auth_w,
        ));
    }
    out
}

/// The served OpenAPI document — the real router's, not a re-derived one.
async fn spec() -> Value {
    let (app, _db) = common::app_and_db().await;
    common::send(&app, "GET", "/api-docs/openapi.json", None, None)
        .await
        .body
}

/// Split the README on the generated-block markers: (before, block, after).
fn split_readme(readme: &str) -> (&str, &str, &str) {
    let (before, rest) = readme
        .split_once(BEGIN)
        .unwrap_or_else(|| panic!("README.md has no `{BEGIN}` marker"));
    let (block, after) = rest
        .split_once(END)
        .unwrap_or_else(|| panic!("README.md has no `{END}` marker"));
    (before, block, after)
}

/// Compared (and written) whitespace-normalised: trailing spaces stripped per
/// line, surrounding blank lines ignored. Cell padding still has to match,
/// which keeps the checked-in table readable as a table.
fn normalize(block: &str) -> String {
    block
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[tokio::test]
async fn readme_endpoint_table_matches_the_spec() {
    let table = render(&spec().await);
    let path = repo().join("README.md");
    let readme = std::fs::read_to_string(&path).expect("read README.md");
    let (before, block, after) = split_readme(&readme);

    if std::env::var_os("UPDATE_DOCS").is_some() {
        let updated = format!("{before}{BEGIN}\n\n{}\n\n{END}{after}", normalize(&table));
        if updated != readme {
            std::fs::write(&path, updated).expect("write README.md");
            eprintln!("README.md endpoint table regenerated");
        }
        return;
    }

    // Reported as the first differing line, not as two 275-row tables: the
    // whole-table dump was ~200KB of scrollback for a one-cell drift.
    let (have, want) = (normalize(block), normalize(&table));
    let (have, want): (Vec<&str>, Vec<&str>) = (have.lines().collect(), want.lines().collect());
    if let Some(i) = (0..have.len().max(want.len())).find(|&i| have.get(i) != want.get(i)) {
        panic!(
            "README.md's endpoint table is stale at block line {}:\n  README: {}\n  spec:   {}\n\
             Regenerate with `UPDATE_DOCS=1 cargo test --test doc_sync`. The cells come from the \
             handlers' `#[utoipa::path]` annotations and doc comments; fix them there, never in \
             the README.",
            i + 1,
            have.get(i).unwrap_or(&"<line missing>"),
            want.get(i).unwrap_or(&"<line missing>"),
        );
    }
}

/// Every plain `.route("…", …)` registered by production code, as (source
/// file, path relative to its nest prefix). `#[cfg(test)]` modules are cut
/// out first — `src/ai/server.rs` mounts throwaway routers in its own tests.
/// Only test *modules* are cut, so a `.route()` inside a `#[cfg(test)] fn`
/// would be reported as untabled; none exists, and the fix is to move it into
/// the file's `mod tests`.
fn plain_routes() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (file, src) in src_files() {
        // A `#[cfg(test)] mod … { … }` at column 0, so its closing `}` is the
        // next column-0 `}`. Brace counting is not an option: the sources are
        // full of `{id}` path literals and `{}` format strings.
        let mut kept = Vec::new();
        let mut lines = src.lines().peekable();
        while let Some(line) = lines.next() {
            if line == "#[cfg(test)]" && lines.peek().is_some_and(|l| l.starts_with("mod ")) {
                for l in lines.by_ref() {
                    if l == "}" {
                        break;
                    }
                }
                continue;
            }
            kept.push(line);
        }
        let src = kept.join("\n");

        let mut at = 0;
        while let Some(hit) = src[at..].find(".route(") {
            let open = at + hit + ".route".len();
            let lit = src[open..].find('"').expect("a route path literal") + open + 1;
            let end = src[lit..].find('"').expect("closing quote") + lit;
            out.push((file.clone(), src[lit..end].to_string()));
            at = open + 1;
        }
    }
    out
}

/// Hazard the generated table cannot see on its own: a route registered with
/// `.route()` carries no `#[utoipa::path]`, so it never reaches `spec["paths"]`
/// and never becomes a row unless someone remembered [`STATIC_ROWS`]. Assert
/// every production `.route()` is accounted for by one of the two.
#[tokio::test]
async fn every_plain_route_is_in_the_table() {
    let spec = spec().await;
    let known: Vec<String> = spec["paths"]
        .as_object()
        .expect("the served spec has a paths object")
        .keys()
        .cloned()
        .chain(STATIC_ROWS.iter().map(|(_, p, ..)| p.to_string()))
        .collect();
    for (file, rel) in plain_routes() {
        // Suffix match on the nest-relative path, the same key `handlers()`
        // uses, so this needs no model of how `lib.rs` nests routers.
        assert!(
            known.iter().any(|p| p == &rel || p.ends_with(&rel)),
            "`{file}` registers `{rel}` with a plain `.route()`, so it is in no \
             OpenAPI path and in no STATIC_ROWS row — it would be missing from \
             the README's endpoint table entirely. Give the handler a \
             `#[utoipa::path]`, or (for a WebSocket/doc route the spec cannot \
             carry) add its row to STATIC_ROWS in this file."
        );
    }
}

/// The other half: [`STATIC_ROWS`] is hand-written, so a row whose route was
/// renamed or deleted would sit in the README forever. Drive the real router
/// and let it answer. Only `404`/`405` refute existence — every one of these
/// is authenticated or a WebSocket upgrade, so `401`/`400`/`426`/a Swagger
/// redirect all prove the route is there.
#[tokio::test]
async fn every_static_row_is_really_routed() {
    let (app, _db) = common::app_and_db().await;
    for (method, path, ..) in STATIC_ROWS {
        let uri = path.replace("{id}", "nosuchid");
        let status = common::send(&app, method, &uri, None, None).await.status;
        assert!(
            !matches!(status.as_u16(), 404 | 405),
            "STATIC_ROWS claims `{method} {path}`, but the built router answers \
             {status} for `{uri}` — the route was renamed or removed and the \
             README row is stale. Fix STATIC_ROWS in this file."
        );
    }
}
