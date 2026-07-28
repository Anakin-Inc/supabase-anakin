#[allow(warnings)]
mod bindings;
use serde_json::Value as JsonValue;

use bindings::{
    exports::supabase::wrappers::routines::Guest,
    supabase::wrappers::{
        http, stats,
        types::{
            Cell, Column, Context, FdwError, FdwResult, ImportForeignSchemaStmt, OptionsType, Row,
            Value,
        },
        utils,
    },
};

#[derive(Debug, Default)]
struct AnakinSearchFdw {
    base_url: String,
    headers: Vec<(String, String)>,
    // the search prompt supplied via `where prompt = '...'`, echoed back on every row
    prompt: String,
    // the `id` field of the POST /search response
    search_id: String,
    // buffered result items from the single synchronous /search call
    src_rows: Vec<JsonValue>,
    src_idx: usize,
}

// pointer for the static FDW instance
static mut INSTANCE: *mut AnakinSearchFdw = std::ptr::null_mut::<AnakinSearchFdw>();
static FDW_NAME: &str = "AnakinSearchFdw";

// Anakin SDK default (anakin-py client.py: `def search(self, prompt, *, limit: int = 5)`)
// used when the query has no `limit` clause to push down.
static DEFAULT_LIMIT: i64 = 5;

impl AnakinSearchFdw {
    fn init_instance() {
        let instance = Self::default();
        unsafe {
            INSTANCE = Box::leak(Box::new(instance));
        }
    }

    fn this_mut() -> &'static mut Self {
        unsafe { &mut (*INSTANCE) }
    }

    // `anakin_search` is parameterized by an equality predicate on `prompt`,
    // the same way other Wasm FDWs (e.g. orb_fdw) require an `id =` qual to
    // address a single-resource endpoint. POST /search has no "list all"
    // mode, so a missing predicate is a hard error rather than an empty scan.
    fn require_prompt_qual(ctx: &Context) -> Result<String, FdwError> {
        ctx.get_quals()
            .iter()
            .find(|q| q.field() == "prompt" && q.operator() == "=")
            .and_then(|q| match q.value() {
                Value::Cell(Cell::String(s)) => Some(s),
                _ => None,
            })
            .ok_or_else(|| {
                "anakin_search requires an equality predicate on 'prompt', e.g. \
                 select * from anakin_search where prompt = 'best rust web frameworks'"
                    .to_owned()
            })
    }

    // SQL `limit`/`offset` push down to the API's `limit` body param. Wrappers
    // exposes only the combined upper bound (offset + count) via ctx.get_limit();
    // Postgres still applies the real OFFSET locally afterwards.
    fn limit_for_request(ctx: &Context) -> i64 {
        match ctx.get_limit() {
            Some(l) => (l.offset() + l.count()).max(1),
            None => DEFAULT_LIMIT,
        }
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

    // map one SearchResultItem JSON object + the echoed request context to a
    // target column. `prompt` and `search_id` aren't in the item JSON - they
    // come from the request/response envelope and are repeated on every row.
    fn src_to_cell(&self, src_row: &JsonValue, tgt_col: &Column) -> Result<Option<Cell>, FdwError> {
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
}

fn str_field(src_row: &JsonValue, field: &str) -> Option<Cell> {
    src_row
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| Cell::String(s.to_owned()))
}

impl Guest for AnakinSearchFdw {
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
        this.prompt = Self::require_prompt_qual(ctx)?;
        this.fetch_search_results(ctx)
    }

    fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
        let this = Self::this_mut();

        if this.src_idx >= this.src_rows.len() {
            // no more buffered rows - /search is a single-shot call, there is
            // no cursor/page to follow up on, so the scan just ends here.
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

    fn re_scan(_ctx: &Context) -> FdwResult {
        // same prompt, same buffered results - just rewind the cursor rather
        // than spending another 3-credit /search call.
        let this = Self::this_mut();
        this.src_idx = 0;
        Ok(())
    }

    fn end_scan(_ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
        this.src_rows.clear();
        Ok(())
    }

    fn begin_modify(_ctx: &Context) -> FdwResult {
        Err("anakin_search is a read-only foreign table; insert/update/delete are not supported".to_owned())
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
        let ret = vec![format!(
            r#"create foreign table if not exists anakin_search (
                prompt text,
                search_id text,
                url text,
                title text,
                snippet text,
                date text,
                last_updated text
            )
            server {} options (
                rowid_column 'url'
            )"#,
            stmt.server_name,
        )];
        Ok(ret)
    }
}

bindings::export!(AnakinSearchFdw with_types_in bindings);
