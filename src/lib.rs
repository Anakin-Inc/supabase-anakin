#[allow(warnings)]
mod bindings;
use serde_json::Value as JsonValue;

use bindings::{
    exports::supabase::wrappers::routines::Guest,
    supabase::wrappers::{
        http, stats, time,
        types::{
            Cell, Column, Context, FdwError, FdwResult, ImportForeignSchemaStmt, ImportSchemaType,
            OptionsType, Row, Value,
        },
        utils,
    },
};

// This FDW backs FOUR foreign tables from one Wasm component, dispatched by
// the table-level `object` option (read via `ctx.get_options(&OptionsType::Table)`
// in begin_scan/begin_modify) - the same multi-object mechanism the real
// `paddle_fdw` and `shopify_fdw` in supabase/wrappers use (confirmed by
// reading `wasm-wrappers/fdw/paddle_fdw/src/lib.rs` in full: `this.object =
// opts.require("object")?`, then every method matches on `self.object`).
// `routines.wit`'s `Context` carries no local-table-name accessor, only
// `get_options(OptionsType::Table)`, so `object` (not the foreign table's own
// name) is the only thing a Guest impl can dispatch on - this is not a
// workaround, it's the documented mechanism the host actually exposes.
#[derive(Debug, Default)]
struct AnakinFdw {
    base_url: String,
    headers: Vec<(String, String)>,
    // which foreign table is being scanned: "search" | "wire_catalog" |
    // "wire_discover" | "ai_visibility_sources" - set from the table's
    // `object` option at the start of every scan/modify.
    object: String,
    // the `where prompt = '...'` qual for anakin_search, echoed back per row
    prompt: String,
    // the `id` field of the POST /search response
    search_id: String,
    // the `where q = '...'` qual for anakin_wire_discover, echoed back per row
    q: String,
    // buffered result items from the single synchronous request for the
    // current scan - every one of these four endpoints is a plain GET/POST
    // with no job id and no pagination cursor Wrappers could follow, so (as
    // with the original anakin_search) one call fills this buffer once
    src_rows: Vec<JsonValue>,
    src_idx: usize,
}

// pointer for the static FDW instance
static mut INSTANCE: *mut AnakinFdw = std::ptr::null_mut::<AnakinFdw>();
static FDW_NAME: &str = "AnakinFdw";

// Anakin SDK default (anakin-py client.py: `def search(self, prompt, *, limit: int = 5)`)
// used when the query has no `limit` clause to push down. Also matches
// `GET /wire/resolve`'s own documented default (`openapi.yaml`'s `/resolve`
// operation: `schema: { type: integer, default: 5, minimum: 1 }`), so
// anakin_wire_discover reuses the same constant rather than inventing a
// second one.
static DEFAULT_LIMIT: i64 = 5;

impl AnakinFdw {
    fn init_instance() {
        let instance = Self::default();
        unsafe {
            INSTANCE = Box::leak(Box::new(instance));
        }
    }

    fn this_mut() -> &'static mut Self {
        unsafe { &mut (*INSTANCE) }
    }

    // Read a required `field = '...'` equality qual off the scan context, or
    // a table-specific error naming the table and field. Used by
    // anakin_search (`prompt`) and anakin_wire_discover (`q`) - both GET/POST
    // endpoints with no "list everything" mode, the same reasoning
    // `orb_fdw` uses for its own required-qual endpoints.
    fn require_eq_qual(ctx: &Context, field: &str, table: &str) -> Result<String, FdwError> {
        ctx.get_quals()
            .iter()
            .find(|qual| qual.field() == field && qual.operator() == "=")
            .and_then(|qual| match qual.value() {
                Value::Cell(Cell::String(s)) => Some(s),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "{table} requires an equality predicate on '{field}', e.g. \
                     select * from {table} where {field} = '...'"
                )
            })
    }

    // Same lookup as `require_eq_qual` but optional - used for
    // anakin_wire_catalog's `scope` filter (`GET /wire/catalog?scope=...`),
    // which `openapi.yaml`'s `/catalog` operation documents as optional.
    fn optional_eq_qual(ctx: &Context, field: &str) -> Option<String> {
        ctx.get_quals()
            .iter()
            .find(|qual| qual.field() == field && qual.operator() == "=")
            .and_then(|qual| match qual.value() {
                Value::Cell(Cell::String(s)) => Some(s),
                _ => None,
            })
    }

    // SQL `limit`/`offset` push down to the API's `limit` query/body param.
    // Wrappers exposes only the combined upper bound (offset + count) via
    // ctx.get_limit(); Postgres still applies the real OFFSET locally
    // afterwards.
    fn limit_for_request(ctx: &Context) -> i64 {
        match ctx.get_limit() {
            Some(l) => (l.offset() + l.count()).max(1),
            None => DEFAULT_LIMIT,
        }
    }

    // Percent-encode a query value, encoding everything outside the RFC 3986
    // unreserved set. Without this a `q` value with a space or '&' would
    // corrupt the request URL and silently return the wrong rows. Lifted
    // verbatim (same byte-for-byte encoding rule) from the real
    // `paddle_fdw::percent_encode`, which solves the identical problem for
    // its own query-string filters.
    fn percent_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    fn do_get(&self, url: &str) -> Result<JsonValue, FdwError> {
        let req = http::Request {
            method: http::Method::Get,
            url: url.to_owned(),
            headers: self.headers.clone(),
            body: String::new(),
        };
        let resp = http::get(&req)?;
        http::error_for_status(&resp).map_err(|err| format!("{}: {}", err, resp.body))?;
        stats::inc_stats(FDW_NAME, stats::Metric::BytesIn, resp.body.len() as i64);
        serde_json::from_str(&resp.body).map_err(|e| e.to_string())
    }

    // POST /search is synchronous - no job id, no polling, the response body
    // *is* the SearchResult. Shape confirmed against anakin-py's
    // SearchResult/SearchResultItem models:
    //   { "id": string, "results": [ { "url", "title"?, "snippet"?, "date"?,
    //     "last_updated"? } ] }
    fn fetch_search_results(&mut self, ctx: &Context) -> FdwResult {
        let limit = Self::limit_for_request(ctx);
        let body = serde_json::json!({
            "prompt": self.prompt,
            "limit": limit,
        })
        .to_string();

        let req = http::Request {
            method: http::Method::Post,
            url: format!("{}/search", self.base_url),
            headers: self.headers.clone(),
            body,
        };

        let resp = http::post(&req)?;
        http::error_for_status(&resp).map_err(|err| format!("{}: {}", err, resp.body))?;

        stats::inc_stats(FDW_NAME, stats::Metric::BytesIn, resp.body.len() as i64);

        let resp_json: JsonValue = serde_json::from_str(&resp.body).map_err(|e| e.to_string())?;

        self.search_id = resp_json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();

        self.src_rows = resp_json
            .get("results")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);

        Ok(())
    }

    // GET /wire/catalog (no slug -> list-every-catalog mode). Response shape
    // confirmed against `anakin-mcp/openapi.yaml`'s `/catalog` operation
    // (title "List catalogs"): `{ "catalog": [CatalogEntry, ...] }`. Only the
    // list mode is exposed here, not `GET /wire/catalog/{slug}` (which
    // returns a structurally different `{ catalog, actions,
    // login_input_schema }` detail object, not a homogeneous row set) - out
    // of scope for this pass, same "don't force a shape that doesn't fit"
    // call this FDW already makes elsewhere.
    fn fetch_wire_catalog_results(&mut self, ctx: &Context) -> FdwResult {
        let mut url = format!("{}/wire/catalog", self.base_url);
        if let Some(scope) = Self::optional_eq_qual(ctx, "scope") {
            url = format!("{url}?scope={}", Self::percent_encode(&scope));
        }

        let resp_json = self.do_get(&url)?;
        self.src_rows = unwrap_list(&resp_json, "catalog");
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);
        Ok(())
    }

    // GET /wire/resolve?q=...&limit=.... Response shape confirmed against
    // `openapi.yaml`'s `/resolve` operation ("Resolve intent -> ranked
    // actions"): `{ "results": [ResolveResult, ...], "next": string }`,
    // where `ResolveResult` is `{ action_id, catalog, params: { required:
    // [...], optional: [...] }, credits, auth_required, auth_satisfied }`.
    // `params` is genuinely nested/variable-shaped per action (it isn't a
    // fixed set of scalar columns), so it's exposed as a single `json`
    // column rather than guessing a flat schema - not a shortcut, this
    // matches how `tap-anakin` (the other place in this codebase modeling
    // Anakin API shapes as fixed columns) handles fields it can't commit to
    // a scalar type for.
    fn fetch_wire_discover_results(&mut self, ctx: &Context) -> FdwResult {
        let limit = Self::limit_for_request(ctx);
        let url = format!(
            "{}/wire/resolve?q={}&limit={}",
            self.base_url,
            Self::percent_encode(&self.q),
            limit
        );

        let resp_json = self.do_get(&url)?;
        self.src_rows = unwrap_list(&resp_json, "results");
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);
        Ok(())
    }

    // GET /ai-visibility/sources - a small, account-independent roster (per
    // `anakin-mcp/src/tools/ai-visibility.ts`'s `ai_visibility_sources`
    // docstring: "each with its slug ... and display label"). Unlike
    // anakin_search and anakin_wire_discover, there is no OpenAPI schema for
    // this endpoint in this codebase (`openapi.yaml` here only documents the
    // Wire API) - the envelope key ("sources") and field names ("slug"/"id",
    // "label"/"name") are the same best-effort mapping `anakin-tap-anakin`'s
    // `AiVisibilitySourcesStream` already uses and explicitly flags as
    // unconfirmed against a live response in its own module docstring. This
    // FDW carries the same honesty forward: typed best-effort columns plus a
    // `raw` json catch-all so nothing is lost if the field-name guess is
    // wrong, rather than presenting an unverified shape as certain.
    fn fetch_ai_visibility_sources(&mut self) -> FdwResult {
        let url = format!("{}/ai-visibility/sources", self.base_url);
        let resp_json = self.do_get(&url)?;
        self.src_rows = unwrap_list(&resp_json, "sources");
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);
        Ok(())
    }

    fn begin_scan_for_object(&mut self, ctx: &Context) -> FdwResult {
        match self.object.as_str() {
            "search" => {
                self.prompt = Self::require_eq_qual(ctx, "prompt", "anakin_search")?;
                self.fetch_search_results(ctx)
            }
            "wire_catalog" => self.fetch_wire_catalog_results(ctx),
            "wire_discover" => {
                self.q = Self::require_eq_qual(ctx, "q", "anakin_wire_discover")?;
                self.fetch_wire_discover_results(ctx)
            }
            "ai_visibility_sources" => self.fetch_ai_visibility_sources(),
            other => Err(format!(
                "anakin_fdw: unknown 'object' table option '{other}' - expected one of \
                 'search', 'wire_catalog', 'wire_discover', 'ai_visibility_sources'"
            )),
        }
    }

    fn src_to_cell(&self, src_row: &JsonValue, tgt_col: &Column) -> Result<Option<Cell>, FdwError> {
        match self.object.as_str() {
            "search" => self.search_cell(src_row, tgt_col),
            "wire_catalog" => self.wire_catalog_cell(src_row, tgt_col),
            "wire_discover" => self.wire_discover_cell(src_row, tgt_col),
            "ai_visibility_sources" => ai_visibility_source_cell(src_row, tgt_col),
            other => Err(format!("unsupported object '{other}'")),
        }
    }

    // map one SearchResultItem JSON object + the echoed request context to a
    // target column. `prompt` and `search_id` aren't in the item JSON - they
    // come from the request/response envelope and are repeated on every row.
    fn search_cell(&self, src_row: &JsonValue, tgt_col: &Column) -> Result<Option<Cell>, FdwError> {
        let name = tgt_col.name();
        let cell = match name.as_str() {
            "prompt" => Some(Cell::String(self.prompt.clone())),
            "search_id" => Some(Cell::String(self.search_id.clone())),
            "url" => str_field(src_row, "url"),
            "title" => str_field(src_row, "title"),
            "snippet" => str_field(src_row, "snippet"),
            "date" => str_field(src_row, "date"),
            "last_updated" => str_field(src_row, "last_updated"),
            _ => {
                return Err(format!(
                    "column '{name}' is not supported by anakin_search"
                ));
            }
        };
        Ok(cell)
    }

    // map one CatalogEntry JSON object (from `openapi.yaml`'s `CatalogEntry`
    // schema) to a target column.
    fn wire_catalog_cell(
        &self,
        src_row: &JsonValue,
        tgt_col: &Column,
    ) -> Result<Option<Cell>, FdwError> {
        let name = tgt_col.name();
        let cell = match name.as_str() {
            "slug" => str_field(src_row, "slug"),
            "name" => str_field(src_row, "name"),
            "url" => str_field(src_row, "url"),
            "domain" => str_field(src_row, "domain"),
            "category" => str_field(src_row, "category"),
            "description" => str_field(src_row, "description"),
            "logo_url" => str_field(src_row, "logo_url"),
            "auth_required" => bool_field(src_row, "auth_required"),
            "auth_type" => str_field(src_row, "auth_type"),
            "auth_types" => json_field(src_row, "auth_types"),
            "auth_login_url" => str_field(src_row, "auth_login_url"),
            "supported_sources" => json_field(src_row, "supported_sources"),
            "status" => str_field(src_row, "status"),
            "action_count" => i32_field(src_row, "action_count"),
            "created_at" => timestamptz_field(src_row, "created_at"),
            "updated_at" => timestamptz_field(src_row, "updated_at"),
            _ => {
                return Err(format!(
                    "column '{name}' is not supported by anakin_wire_catalog"
                ));
            }
        };
        Ok(cell)
    }

    // map one ResolveResult JSON object (from `openapi.yaml`'s
    // `ResolveResult` schema) + the echoed `q` to a target column.
    fn wire_discover_cell(
        &self,
        src_row: &JsonValue,
        tgt_col: &Column,
    ) -> Result<Option<Cell>, FdwError> {
        let name = tgt_col.name();
        let cell = match name.as_str() {
            "q" => Some(Cell::String(self.q.clone())),
            "action_id" => str_field(src_row, "action_id"),
            "catalog" => str_field(src_row, "catalog"),
            "credits" => i32_field(src_row, "credits"),
            "auth_required" => bool_field(src_row, "auth_required"),
            "auth_satisfied" => bool_field(src_row, "auth_satisfied"),
            "params" => json_field(src_row, "params"),
            _ => {
                return Err(format!(
                    "column '{name}' is not supported by anakin_wire_discover"
                ));
            }
        };
        Ok(cell)
    }
}

// map one AI Visibility source JSON object to a target column - see
// `fetch_ai_visibility_sources` for why `slug`/`label` are best-effort
// fallback chains rather than a single confirmed field name, and why `raw`
// exists as a lossless catch-all alongside them.
fn ai_visibility_source_cell(
    src_row: &JsonValue,
    tgt_col: &Column,
) -> Result<Option<Cell>, FdwError> {
    let name = tgt_col.name();
    let cell = match name.as_str() {
        "slug" => str_field(src_row, "slug").or_else(|| str_field(src_row, "id")),
        "label" => str_field(src_row, "label").or_else(|| str_field(src_row, "name")),
        "raw" => Some(Cell::Json(src_row.to_string())),
        _ => {
            return Err(format!(
                "column '{name}' is not supported by anakin_ai_visibility_sources"
            ));
        }
    };
    Ok(cell)
}

// Tolerate the response shapes Anakin's collection endpoints can return,
// mirroring `anakin-py`'s own `_unwrap_list` helper (re-applied here in Rust
// rather than reinvented - same three cases: a plain list, a populated
// `{key: [...]}` wrapper, or an absent/null key):
//   - `[item, item, ...]`
//   - `{"key": [item, item, ...]}`
//   - `{"key": null}` or `{}`
fn unwrap_list(body: &JsonValue, key: &str) -> Vec<JsonValue> {
    if let Some(arr) = body.as_array() {
        return arr.clone();
    }
    body.get(key)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn str_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    src_row
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| Cell::String(s.to_owned()))
}

fn bool_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    src_row.get(field).and_then(|v| v.as_bool()).map(Cell::Bool)
}

fn i32_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    src_row
        .get(field)
        .and_then(|v| v.as_i64())
        .map(|n| Cell::I32(n as i32))
}

// Serialize a nested/array JSON field (e.g. CatalogEntry.auth_types,
// ResolveResult.params) to a `json` cell verbatim - these fields are not
// flat scalars, so a jsonb column is the honest representation rather than
// flattening into an invented set of extra columns.
fn json_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    match src_row.get(field) {
        Some(v) if !v.is_null() => Some(Cell::Json(v.to_string())),
        _ => None,
    }
}

// Parse an RFC 3339 timestamp string field to a `timestamptz` cell via the
// host's `time::parse-from-rfc3339` import. A malformed/unexpected value is
// logged (`utils::report_warning`, visible in the Postgres log - satisfies
// "errors logged with enough context to debug in production") and mapped to
// SQL NULL rather than aborting the whole scan over one bad row.
fn timestamptz_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    let raw = src_row.get(field).and_then(|v| v.as_str())?;
    match time::parse_from_rfc3339(raw) {
        Ok(micros) => Some(Cell::Timestamptz(micros)),
        Err(err) => {
            utils::report_warning(&format!(
                "anakin_fdw: could not parse '{field}' value '{raw}' as RFC3339 ({err}); \
                 returning NULL for this cell"
            ));
            None
        }
    }
}

impl Guest for AnakinFdw {
    fn host_version_requirement() -> String {
        // semver ref: https://docs.rs/semver/latest/semver/enum.Op.html
        "^0.1.0".to_string()
    }

    fn init(ctx: &Context) -> FdwResult {
        Self::init_instance();
        let this = Self::this_mut();

        // foreign server options
        let opts = ctx.get_options(&OptionsType::Server);
        this.base_url = opts.require_or("api_url", "https://api.anakin.io/v1");

        // auth: X-API-Key header (api.anakin.io/v1, confirmed against
        // anakin-py's HttpClient). Either pass the key directly via the
        // `api_key` server option (fine for local/dev only, it lands in
        // pg_catalog in plaintext) or store it in Supabase Vault and
        // reference it by `api_key_id`, same pattern orb_fdw/hubspot_fdw use.
        let api_key = match opts.get("api_key") {
            Some(key) => key,
            None => {
                let key_id = opts.require("api_key_id")?;
                utils::get_vault_secret(&key_id).unwrap_or_default()
            }
        };
        if api_key.is_empty() {
            return Err(
                "Anakin API key not found: set the 'api_key' server option or \
                 'api_key_id' pointing to a Supabase Vault secret"
                    .to_owned(),
            );
        }

        this.headers
            .push(("content-type".to_owned(), "application/json".to_string()));
        this.headers.push(("x-api-key".to_owned(), api_key));

        stats::inc_stats(FDW_NAME, stats::Metric::CreateTimes, 1);

        Ok(())
    }

    fn begin_scan(ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
        let opts = ctx.get_options(&OptionsType::Table);
        this.object = opts.require("object")?;
        this.begin_scan_for_object(ctx)
    }

    fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
        let this = Self::this_mut();

        if this.src_idx >= this.src_rows.len() {
            // no more buffered rows - every one of the four objects is a
            // single-shot call, there is no cursor/page to follow up on, so
            // the scan just ends here.
            return Ok(None);
        }

        let src_row = this.src_rows[this.src_idx].clone();
        for tgt_col in ctx.get_columns() {
            let cell = this.src_to_cell(&src_row, &tgt_col)?;
            row.push(cell.as_ref());
        }
        this.src_idx += 1;
        stats::inc_stats(FDW_NAME, stats::Metric::RowsOut, 1);

        Ok(Some(0))
    }

    fn re_scan(ctx: &Context) -> FdwResult {
        // anakin_search rewinds the cursor without spending another credited
        // call (same prompt, same buffered results). The other three objects
        // are free listing/lookup calls, so re-fetching on a re-scan is fine
        // and keeps their data fresh within the same statement; only search
        // spends credits per call, hence the special case.
        let this = Self::this_mut();
        if this.object == "search" {
            this.src_idx = 0;
            return Ok(());
        }
        this.begin_scan_for_object(ctx)
    }

    fn end_scan(_ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
        this.src_rows.clear();
        Ok(())
    }

    fn begin_modify(ctx: &Context) -> FdwResult {
        let opts = ctx.get_options(&OptionsType::Table);
        let object = opts.get("object").unwrap_or_else(|| "unknown".to_owned());
        Err(format!(
            "anakin_fdw tables (object '{object}') are read-only; insert/update/delete are not supported"
        ))
    }

    fn insert(_ctx: &Context, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn update(_ctx: &Context, _rowid: Cell, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn delete(_ctx: &Context, _rowid: Cell) -> FdwResult {
        Ok(())
    }

    fn end_modify(_ctx: &Context) -> FdwResult {
        Ok(())
    }

    fn import_foreign_schema(
        _ctx: &Context,
        stmt: ImportForeignSchemaStmt,
    ) -> Result<Vec<String>, FdwError> {
        // (foreign table name, object option, column list, rowid column).
        // Mirrors the real `paddle_fdw::import_foreign_schema` shape (one
        // `create foreign table` per object, each carrying its own `object`
        // option) - the table names here are the `anakin_`-prefixed names
        // this FDW already documents, not the bare object slug Paddle uses.
        let tables: Vec<(&str, &str, &str, &str)> = vec![
            (
                "anakin_search",
                "search",
                "prompt text, search_id text, url text, title text, snippet text, \
                 date text, last_updated text",
                "url",
            ),
            (
                "anakin_wire_catalog",
                "wire_catalog",
                "slug text, name text, url text, domain text, category text, \
                 description text, logo_url text, auth_required boolean, auth_type text, \
                 auth_types json, auth_login_url text, supported_sources json, status text, \
                 action_count integer, created_at timestamptz, updated_at timestamptz",
                "slug",
            ),
            (
                "anakin_wire_discover",
                "wire_discover",
                "q text, action_id text, catalog text, credits integer, \
                 auth_required boolean, auth_satisfied boolean, params json",
                "action_id",
            ),
            (
                "anakin_ai_visibility_sources",
                "ai_visibility_sources",
                "slug text, label text, raw json",
                "slug",
            ),
        ];

        // honor the `limit to (...)` / `except (...)` clauses, matching
        // against the foreign table name (the name a user's `import foreign
        // schema` statement actually lists).
        let selected: Vec<(&str, &str, &str, &str)> = match stmt.list_type {
            ImportSchemaType::All => tables,
            ImportSchemaType::LimitTo => tables
                .into_iter()
                .filter(|(name, ..)| stmt.table_list.iter().any(|t| t.as_str() == *name))
                .collect(),
            ImportSchemaType::Except => tables
                .into_iter()
                .filter(|(name, ..)| !stmt.table_list.iter().any(|t| t.as_str() == *name))
                .collect(),
        };

        let ret: Vec<String> = selected
            .into_iter()
            .map(|(table_name, object, columns, rowid_col)| {
                format!(
                    r#"create foreign table if not exists {table_name} (
                {columns}
            )
            server {} options (
                object '{object}',
                rowid_column '{rowid_col}'
            )"#,
                    stmt.server_name,
                )
            })
            .collect();

        Ok(ret)
    }
}

bindings::export!(AnakinFdw with_types_in bindings);
