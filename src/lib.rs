#[allow(warnings)]
mod bindings;
use serde_json::{json, Map as JsonMap, Value as JsonValue};

use bindings::{
    exports::supabase::wrappers::routines::Guest,
    supabase::wrappers::{
        http, stats, time,
        types::{
            Cell, Column, Context, FdwError, FdwResult, ImportForeignSchemaStmt, ImportSchemaType,
            OptionsType, Row, TypeOid, Value,
        },
        utils,
    },
};

// This FDW backs SIX foreign tables from one Wasm component, dispatched by
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
    // which foreign table is being scanned: "scrape" | "map" | "search" |
    // "wire_catalog" | "wire_discover" | "ai_visibility_sources" - set from the table's
    // `object` option at the start of every scan/modify.
    object: String,
    // the `where prompt = '...'` qual for anakin_search, echoed back per row
    prompt: String,
    // the `id` field of the POST /search response
    search_id: String,
    // the `where q = '...'` qual for anakin_wire_discover, echoed back per row
    q: String,
    // buffered result rows for the current scan - every object fills this
    // buffer once in begin_scan (there is no pagination cursor to follow)
    src_rows: Vec<JsonValue>,
    src_idx: usize,
    // how long to poll async jobs (map, and scrapes that outlive the inline wait)
    poll_timeout_secs: i64,
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

static USER_AGENT: &str = concat!("supabase-anakin-fdw/", env!("CARGO_PKG_VERSION"));
const POLL_INTERVAL_MS: u64 = 2_000;
const DEFAULT_POLL_TIMEOUT_SECS: i64 = 180;

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

    fn do_post(&self, path: &str, body: &JsonValue) -> Result<JsonValue, FdwError> {
        let req = http::Request {
            method: http::Method::Post,
            url: format!("{}{}", self.base_url, path),
            headers: self.headers.clone(),
            body: body.to_string(),
        };
        let resp = http::post(&req)?;
        http::error_for_status(&resp).map_err(|err| format!("{}: {}", err, resp.body))?;
        stats::inc_stats(FDW_NAME, stats::Metric::BytesIn, resp.body.len() as i64);
        serde_json::from_str(&resp.body).map_err(|e| e.to_string())
    }

    // GET `path` until the job reaches a terminal status or poll_timeout_secs
    // runs out. The host's `time::sleep` blocks the backend between polls, so
    // a slow job holds the query open (it can be cancelled like any query).
    fn poll_job(&self, path: &str, mut body: JsonValue) -> Result<JsonValue, FdwError> {
        let deadline = time::epoch_secs() + self.poll_timeout_secs;
        loop {
            if let Some("completed" | "failed") = body.get("status").and_then(|v| v.as_str()) {
                return Ok(body);
            }
            if time::epoch_secs() >= deadline {
                let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                return Err(format!(
                    "Anakin job {id} did not finish within {}s (raise the \
                     poll_timeout_secs server option)",
                    self.poll_timeout_secs
                ));
            }
            time::sleep(POLL_INTERVAL_MS);
            body = self.do_get(&format!("{}{}", self.base_url, path))?;
        }
    }

    // POST /url-scraper/scrape blocks up to ~90s and returns the finished
    // page; if the page takes longer it returns the job id and a pending
    // status, and we poll GET /url-scraper/{id}. Works without an API key on
    // the keyless free tier. One row per URL in `where url = ...` / `url in
    // (...)`; `country` and `use_browser` filters are passed through.
    fn fetch_scrape_results(&mut self, ctx: &Context) -> FdwResult {
        let opts = ctx.get_options(&OptionsType::Table);
        let formats: Vec<String> = opts
            .require_or("formats", "markdown")
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        let mut country = opts.get("country");
        let mut use_browser = opts.get("use_browser").as_deref().and_then(parse_bool);
        let mut urls = Vec::new();
        for qual in ctx.get_quals() {
            if qual.operator() != "=" {
                continue;
            }
            match (qual.field().as_str(), qual.value()) {
                ("url", value) => urls = qual_strings(&value),
                ("country", Value::Cell(Cell::String(s))) => country = Some(s),
                ("use_browser", Value::Cell(Cell::Bool(b))) => use_browser = Some(b),
                _ => {}
            }
        }
        if urls.is_empty() {
            return Err(
                "anakin_scrape requires an equality predicate on 'url', e.g. \
                        select * from anakin_scrape where url = 'https://example.com' \
                        (or url in (...))"
                    .to_owned(),
            );
        }

        self.src_rows.clear();
        for url in urls {
            let mut req = json!({ "url": url, "formats": formats });
            if let Some(country) = &country {
                req["country"] = json!(country);
            }
            if let Some(use_browser) = use_browser {
                req["useBrowser"] = json!(use_browser);
            }
            let mut resp = self.do_post("/url-scraper/scrape", &req)?;
            let job_id = resp
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            if let Some(id) = job_id {
                resp = self.poll_job(&format!("/url-scraper/{id}"), resp)?;
            }

            let mut row = snake_case_keys(&resp);
            // echo the filter values so Postgres' local recheck of the WHERE
            // clause keeps the row (the API reports final_url separately)
            row.insert("url".to_owned(), json!(url));
            if let Some(country) = &country {
                row.insert("country".to_owned(), json!(country));
            }
            row.insert(
                "use_browser".to_owned(),
                json!(use_browser.unwrap_or(false)),
            );
            row.insert("attrs".to_owned(), resp);
            self.src_rows.push(JsonValue::Object(row));
        }
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);
        Ok(())
    }

    // POST /map submits a job, GET /map/{id} returns `links` and
    // `externalLinks` once completed. One row per discovered URL.
    fn fetch_map_results(&mut self, ctx: &Context) -> FdwResult {
        let url = Self::require_eq_qual(ctx, "url", "anakin_map")?;
        let opts = ctx.get_options(&OptionsType::Table);
        let mut req = json!({ "url": url });
        if let Some(limit) = opts.get("limit").and_then(|s| s.parse::<i64>().ok()) {
            req["limit"] = json!(limit);
        }
        if let Some(b) = opts
            .get("include_subdomains")
            .as_deref()
            .and_then(parse_bool)
        {
            req["includeSubdomains"] = json!(b);
        }
        if let Some(search) = opts.get("search") {
            req["search"] = json!(search);
        }

        let submitted = self.do_post("/map", &req)?;
        let id = submitted
            .get("id")
            .or_else(|| submitted.get("job_id"))
            .and_then(|v| v.as_str())
            .ok_or("Anakin map response has no job id")?
            .to_owned();
        let resp = self.poll_job(&format!("/map/{id}"), submitted)?;
        if resp.get("status").and_then(|v| v.as_str()) == Some("failed") {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Anakin map job {id} failed: {err}"));
        }

        self.src_rows.clear();
        for (key, external) in [("links", false), ("externalLinks", true)] {
            let links = resp.get(key).and_then(|v| v.as_array());
            for link in links.into_iter().flatten().filter_map(|v| v.as_str()) {
                self.src_rows.push(json!({
                    "url": url,
                    "link": link,
                    "is_external": external,
                    "job_id": id,
                }));
            }
        }
        self.src_idx = 0;

        stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.src_rows.len() as i64);
        Ok(())
    }

    fn begin_scan_for_object(&mut self, ctx: &Context) -> FdwResult {
        match self.object.as_str() {
            "scrape" => self.fetch_scrape_results(ctx),
            "map" => self.fetch_map_results(ctx),
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
                 'scrape', 'map', 'search', 'wire_catalog', 'wire_discover', \
                 'ai_visibility_sources'"
            )),
        }
    }

    fn src_to_cell(&self, src_row: &JsonValue, tgt_col: &Column) -> Result<Option<Cell>, FdwError> {
        match self.object.as_str() {
            "scrape" | "map" => json_to_cell(src_row, tgt_col),
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
                return Err(format!("column '{name}' is not supported by anakin_search"));
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

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "t" | "yes" | "on" | "1" => Some(true),
        "false" | "f" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

// string values of an `=` qual, including `in (...)` / `= any(...)` lists
fn qual_strings(value: &Value) -> Vec<String> {
    let one = |c: &Cell| match c {
        Cell::String(s) => Some(s.clone()),
        _ => None,
    };
    match value {
        Value::Cell(c) => one(c).into_iter().collect(),
        Value::Array(cells) => cells.iter().filter_map(one).collect(),
    }
}

// API responses are camelCase, columns are snake_case
fn camel_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn snake_case_keys(src: &JsonValue) -> JsonMap<String, JsonValue> {
    let mut out = JsonMap::new();
    if let Some(obj) = src.as_object() {
        for (k, v) in obj {
            out.insert(camel_to_snake(k), v.clone());
        }
    }
    out
}

// convert a column of a normalized row (anakin_scrape, anakin_map) into a
// cell of the column's declared type, so users can pick text/jsonb/etc.
fn json_to_cell(src_row: &JsonValue, tgt_col: &Column) -> Result<Option<Cell>, FdwError> {
    let src = match src_row.get(tgt_col.name()) {
        Some(v) if !v.is_null() => v,
        _ => return Ok(None),
    };
    let cell = match tgt_col.type_oid() {
        TypeOid::Bool => src.as_bool().map(Cell::Bool),
        TypeOid::I16 => src.as_i64().map(|v| Cell::I16(v as i16)),
        TypeOid::I32 => src.as_i64().map(|v| Cell::I32(v as i32)),
        TypeOid::I64 => src.as_i64().map(Cell::I64),
        TypeOid::F32 => src.as_f64().map(|v| Cell::F32(v as f32)),
        TypeOid::F64 => src.as_f64().map(Cell::F64),
        TypeOid::Numeric => src.as_f64().map(Cell::Numeric),
        TypeOid::String => Some(Cell::String(match src.as_str() {
            Some(s) => s.to_owned(),
            None => src.to_string(),
        })),
        TypeOid::Timestamp => src
            .as_str()
            .and_then(|s| time::parse_from_rfc3339(s).ok())
            .map(Cell::Timestamp),
        TypeOid::Timestamptz => src
            .as_str()
            .and_then(|s| time::parse_from_rfc3339(s).ok())
            .map(Cell::Timestamptz),
        TypeOid::Json => Some(Cell::Json(src.to_string())),
        _ => {
            return Err(format!(
                "column '{}' has an unsupported type; use text, bigint, boolean, \
                 timestamptz or jsonb",
                tgt_col.name()
            ))
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
        //
        // The key is optional: anakin_scrape works without one on the keyless
        // free tier. Every other table returns the API's own 401 message.
        let api_key = match (
            opts.get("api_key"),
            opts.get("api_key_id"),
            opts.get("api_key_name"),
        ) {
            (Some(key), _, _) => key,
            (None, Some(key_id), _) => utils::get_vault_secret(&key_id)
                .ok_or(format!("Vault secret '{key_id}' (api_key_id) not found"))?,
            (None, None, Some(name)) => utils::get_vault_secret_by_name(&name)
                .ok_or(format!("Vault secret '{name}' (api_key_name) not found"))?,
            (None, None, None) => String::new(),
        };

        this.base_url = this.base_url.trim_end_matches('/').to_owned();
        this.headers
            .push(("content-type".to_owned(), "application/json".to_string()));
        this.headers
            .push(("user-agent".to_owned(), USER_AGENT.to_owned()));
        if !api_key.trim().is_empty() {
            this.headers
                .push(("x-api-key".to_owned(), api_key.trim().to_owned()));
        }

        this.poll_timeout_secs = opts
            .get("poll_timeout_secs")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_POLL_TIMEOUT_SECS);

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
            // no more buffered rows - every object is a
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
        // anakin_scrape, anakin_map and anakin_search rewind the cursor
        // without spending another credited call (same quals, same buffered
        // results). The other three objects are free listing/lookup calls,
        // so re-fetching on a re-scan is fine.
        let this = Self::this_mut();
        if matches!(this.object.as_str(), "scrape" | "map" | "search") {
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
                "anakin_scrape",
                "scrape",
                "url text, final_url text, status text, status_code bigint, markdown text, \
                 html text, cleaned_html text, links jsonb, images jsonb, summary text, \
                 generated_json jsonb, country text, use_browser boolean, cached boolean, \
                 duration_ms bigint, created_at timestamptz, completed_at timestamptz, \
                 error text, id text, attrs jsonb",
                "url",
            ),
            (
                "anakin_map",
                "map",
                "url text, link text, is_external boolean, job_id text",
                "link",
            ),
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
