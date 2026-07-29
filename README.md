# supabase-anakin

A [Supabase Wrappers](https://supabase.com/docs/guides/database/extensions/wrappers)
Wasm Foreign Data Wrapper for [Anakin](https://anakin.io) — query Anakin's
AI-powered web search and Wire automation catalog directly from Postgres/SQL.

```sql
select url, title, snippet
from anakin_search
where prompt = 'best rust web frameworks 2026'
limit 5;

select slug, name, action_count
from anakin_wire_catalog
where domain like '%walmart%';
```

## What this is

A [WebAssembly Component Model](https://component-model.bytecodealliance.org/)
guest implementing Supabase Wrappers' `supabase:wrappers/routines` interface
(WIT package `supabase:wrappers@0.2.0`) against the Anakin endpoints that are
synchronous end-to-end (no job id, no polling) — the only shape a FDW's
per-row `iter-scan` call can actually represent cleanly. The other Anakin
endpoints (`scrape`, `map`, `crawl`, `agentic_search`, `wire` action
execution, `ai_visibility_search`) are submit-then-poll jobs; the Wrappers
Wasm host interface (`wit/v2`, see `supabase-wrappers-wit/routines.wit`) has
no async/job primitive, so mapping those to synchronous foreign-table scans
would mean polling inside a single `iter-scan` call with no way to yield back
to Postgres — the same "scope down to sync-only" call made for
`fivetran-anakin` and `looker-studio-anakin`.

One Wasm component backs **four** foreign tables, dispatched by a per-table
`object` server/table option — the same multi-object mechanism the real
`paddle_fdw` and `shopify_fdw` in the `supabase/wrappers` monorepo use
(`ctx.get_options(&OptionsType::Table).require("object")` in `begin_scan`),
not a workaround: `routines.wit`'s `Context` has no local-table-name accessor,
only `get_options`, so `object` is the mechanism the host actually exposes
for this.

| table                          | Anakin endpoint            | qual required |
| ------------------------------- | -------------------------- | -------------- |
| `anakin_search`                 | `POST /search`              | `prompt =`     |
| `anakin_wire_catalog`           | `GET /wire/catalog`         | none (`scope =` optional) |
| `anakin_wire_discover`          | `GET /wire/resolve`         | `q =`          |
| `anakin_ai_visibility_sources`  | `GET /ai-visibility/sources`| none           |

Every other Wire/AI-Visibility read endpoint was re-checked against
`anakin-mcp/src/tools/wire.ts` and `ai-visibility.ts` and is *not* sync-only:
`wire_read_action`/`wire_write_action` (`POST /wire/task` + poll job),
`ai_visibility_search` (`POST /ai-visibility/search` + poll), `wire_login`,
`wire_build` — none of these fit the same-request-same-response shape a
foreign-table scan needs, so they remain out of scope, same reasoning as
`scrape`/`map`/`crawl`/`agentic_search`. `GET /wire/catalog/{slug}` (a single
catalog's full action list) was also considered and left out: its response
(`{ catalog, actions, login_input_schema }`) is a compound detail object, not
a homogeneous row set like `GET /wire/catalog`'s list mode, so it doesn't
decompose into one table's column set without inventing a shape the API
doesn't return.

## Project layout

```
Cargo.toml                 crate-type = ["cdylib"], cargo-component metadata
.cargo/config.toml          target = "wasm32-unknown-unknown"
wit/world.wit                this component's own world (anakin:anakin-fdw@0.2.0)
supabase-wrappers-wit/        the host-provided WIT, copied verbatim from
                               supabase/wrappers wasm-wrappers/wit/v2
src/lib.rs                   the Guest implementation (AnakinFdw)
```

## Tables

### `anakin_search`

Parameterized by an equality predicate on `prompt` — the same way other Wasm
FDWs require an `id =` qual to address a single-resource endpoint, since
`POST /search` has no "list everything" mode.

| column         | type | source                                          |
| -------------- | ---- | ------------------------------------------------ |
| `prompt`       | text | echoed back from the `where prompt = '...'` qual |
| `search_id`    | text | `SearchResult.id`                                 |
| `url`          | text | `SearchResultItem.url`                            |
| `title`        | text | `SearchResultItem.title`                          |
| `snippet`      | text | `SearchResultItem.snippet`                        |
| `date`         | text | `SearchResultItem.date`                           |
| `last_updated` | text | `SearchResultItem.last_updated`                   |

A `limit` clause pushes down to the API's `limit` body parameter (default 5,
matching `anakin-py`'s `Anakin.search()` default); `offset` is still applied
by Postgres locally, same as every other pushdown-limited Wasm FDW in the
Wrappers catalog.

### `anakin_wire_catalog`

`GET /wire/catalog` (list-every-catalog mode, no `slug`). Shape confirmed
against `anakin-mcp/openapi.yaml`'s `CatalogEntry` schema. An optional
`scope = 'my'` / `scope = 'global'` predicate pushes down to the API's
`?scope=` filter.

| column               | type        | source (`CatalogEntry`)     |
| -------------------- | ----------- | ---------------------------- |
| `slug`                | text        | `slug`                        |
| `name`                | text        | `name`                        |
| `url`                 | text        | `url`                         |
| `domain`              | text        | `domain`                      |
| `category`            | text        | `category`                    |
| `description`         | text        | `description`                 |
| `logo_url`            | text        | `logo_url`                    |
| `auth_required`       | boolean     | `auth_required`               |
| `auth_type`           | text        | `auth_type`                   |
| `auth_types`          | json        | `auth_types` (array, verbatim)|
| `auth_login_url`      | text        | `auth_login_url`              |
| `supported_sources`   | json        | `supported_sources` (array)   |
| `status`              | text        | `status`                      |
| `action_count`        | integer     | `action_count`                |
| `created_at`          | timestamptz | `created_at` (RFC 3339)       |
| `updated_at`          | timestamptz | `updated_at` (RFC 3339)       |

### `anakin_wire_discover`

`GET /wire/resolve?q=...&limit=...`. Parameterized by a required equality
predicate on `q`, same reasoning as `anakin_search`'s `prompt`. Shape
confirmed against `openapi.yaml`'s `ResolveResult` schema. `params` is
genuinely nested/variable per action (`{ required: [...], optional: [...] }`)
so it's exposed as `json` rather than an invented flat schema.

| column           | type    | source (`ResolveResult`)              |
| ---------------- | ------- | --------------------------------------- |
| `q`               | text    | echoed back from `where q = '...'`       |
| `action_id`       | text    | `action_id`                              |
| `catalog`         | text    | `catalog` (slug)                         |
| `credits`         | integer | `credits`                                |
| `auth_required`   | boolean | `auth_required`                          |
| `auth_satisfied`  | boolean | `auth_satisfied` (nullable)              |
| `params`          | json    | `params` (verbatim)                      |

A `limit` clause pushes down the same way as `anakin_search` (default 5,
matching `openapi.yaml`'s documented default for this endpoint).

### `anakin_ai_visibility_sources`

`GET /ai-visibility/sources` — a full, unconditional listing (no qual). No
OpenAPI schema exists for this endpoint in this codebase (`openapi.yaml` here
only covers the Wire API), so `slug`/`label` are the same best-effort
fallback mapping (`slug` or `id`, `label` or `name`) that
`anakin-tap-anakin`'s `AiVisibilitySourcesStream` already uses and flags as
unconfirmed against a live response — carried forward honestly rather than
presented as certain. `raw` is the full source object verbatim, so nothing is
lost if that guess is wrong.

| column  | type | source                                   |
| ------- | ---- | ----------------------------------------- |
| `slug`   | text | `slug` \|\| `id`                           |
| `label`  | text | `label` \|\| `name`                        |
| `raw`    | json | the source object, verbatim                |

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo install cargo-component --locked --version 0.21.1
cargo component build --release --target wasm32-unknown-unknown
```

Produces `target/wasm32-unknown-unknown/release/anakin_fdw.wasm`. Verified
building clean (zero warnings) with this exact command as of 2026-07-29 (the
four-table `v0.2.0` revision, re-verified after the original `v0.1.0`
search-only build on 2026-07-28) — see `SUBMIT.md`'s "Compile-check status"
for the full log and the toolchain note below.

> **cargo-component version matters.** `cargo-component` versions around
> 0.13.x (2024-era) hardcode a target-provisioning step that always requests
> the target by its old name, `wasm32-wasi`. Current Rust toolchains (this
> was built against `rustc 1.95.0`) renamed that target to `wasm32-wasip1`
> and no longer recognize `wasm32-wasi` at all, so old cargo-component
> versions fail before your code is even compiled, with:
> `error: toolchain '...' does not support target 'wasm32-wasi'; did you mean 'wasm32-wasip1'?`
> This is unrelated to the crate's actual build target
> (`wasm32-unknown-unknown`, set in `.cargo/config.toml` and matching the
> real `supabase/wrappers` monorepo) — it's an internal step cargo-component
> runs before invoking `cargo build`. Fix: use cargo-component **0.21.1 or
> newer** (current `max_stable_version` on crates.io as of 2026-07-28), which
> knows the renamed target. Full details, exact error text, and confirmation
> this fix works are in `SUBMIT.md`.

## Install (once built and hosted, e.g. as a GitHub release asset)

```sql
create extension if not exists wrappers with schema extensions;

create foreign data wrapper wasm_wrapper
  handler wasm_fdw_handler
  validator wasm_fdw_validator;

-- store the key in Vault (recommended) and reference it by id, or pass
-- api_key directly (lands in pg_catalog in plaintext - dev/test only)
select vault.create_secret('<Anakin API key>', 'anakin', 'Anakin API key for Wrappers');

create server anakin_server
  foreign data wrapper wasm_wrapper
  options (
    fdw_package_url 'https://github.com/<org>/<repo>/releases/download/v0.2.0/anakin_fdw.wasm',
    fdw_package_name 'anakin:anakin-fdw',
    fdw_package_version '0.2.0',
    fdw_package_checksum '<sha256 of the released .wasm, printed by the release workflow>',
    api_url 'https://api.anakin.io/v1',  -- optional, this is the default
    api_key_id '<key_id from vault.create_secret above>'
  );

-- one Wasm component, four tables — each selected by its `object` option
create foreign table anakin_search (
  prompt text,
  search_id text,
  url text,
  title text,
  snippet text,
  date text,
  last_updated text
)
server anakin_server
options (
  object 'search',
  rowid_column 'url'
);

create foreign table anakin_wire_catalog (
  slug text,
  name text,
  url text,
  domain text,
  category text,
  description text,
  logo_url text,
  auth_required boolean,
  auth_type text,
  auth_types json,
  auth_login_url text,
  supported_sources json,
  status text,
  action_count integer,
  created_at timestamptz,
  updated_at timestamptz
)
server anakin_server
options (
  object 'wire_catalog',
  rowid_column 'slug'
);

create foreign table anakin_wire_discover (
  q text,
  action_id text,
  catalog text,
  credits integer,
  auth_required boolean,
  auth_satisfied boolean,
  params json
)
server anakin_server
options (
  object 'wire_discover',
  rowid_column 'action_id'
);

create foreign table anakin_ai_visibility_sources (
  slug text,
  label text,
  raw json
)
server anakin_server
options (
  object 'ai_visibility_sources',
  rowid_column 'slug'
);
```

Or skip hand-writing the four `create foreign table` statements above with
`import foreign schema` (implemented in `src/lib.rs`'s
`import_foreign_schema`, honors `limit to (...)` / `except (...)`):

```sql
import foreign schema anakin
  from server anakin_server
  into public;
```

Get a free API key at [anakin.io/dashboard](https://anakin.io/dashboard) —
300 credits, no card required. `search` costs 3 credits per call;
`wire_catalog`, `wire_discover`, and `ai_visibility_sources` are free
discovery/listing calls.

## Test

No automated test harness in this directory (Wasm Component FDWs are
exercised through a live Postgres + Wrappers extension + real API key, none
of which exist in a dev sandbox). What was verified instead:

- `cargo component build --release --target wasm32-unknown-unknown` and
  `cargo component check` both complete with zero warnings and zero errors,
  producing a genuine Component Model binary (confirmed by inspecting the
  raw header bytes, not just the exit code).
- Every `Guest` trait method in `src/lib.rs` compiled as-is against the
  `Guest` trait `wit-bindgen` 0.41.0 generated fresh from `wit/world.wit` +
  `supabase-wrappers-wit/routines.wit` — no signature had to be adjusted to
  get it to build, including after adding the three new tables and the
  `object`-based dispatch.

Not verified: an actual HTTP round trip (`POST /search`, `GET /wire/catalog`,
`GET /wire/resolve`, `GET /ai-visibility/sources`) against the live Anakin
API, or a `select` against a real foreign table in Postgres. See `SUBMIT.md`
for the full compile log, the toolchain issue that was hit and fixed along
the way, and what's left for a human with Postgres + API key access.
