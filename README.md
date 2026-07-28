# supabase-anakin

A [Supabase Wrappers](https://supabase.com/docs/guides/database/extensions/wrappers)
Wasm Foreign Data Wrapper for [Anakin](https://anakin.io) — query Anakin's
AI-powered web search directly from Postgres/SQL.

```sql
select url, title, snippet
from anakin_search
where prompt = 'best rust web frameworks 2026'
limit 5;
```

## What this is

A [WebAssembly Component Model](https://component-model.bytecodealliance.org/)
guest implementing Supabase Wrappers' `supabase:wrappers/routines` interface
(WIT package `supabase:wrappers@0.2.0`) against `POST /v1/search` — the one
Anakin endpoint that is synchronous end-to-end (no job id, no polling), which
is what a FDW's per-row `iter-scan` call can actually represent cleanly. The
other Anakin endpoints (`scrape`, `map`, `crawl`, `agentic_search`, `wire`)
are submit-then-poll jobs; the Wrappers Wasm host interface (`wit/v2`, see
`supabase-wrappers-wit/routines.wit`) has no async/job primitive, so mapping
those to synchronous foreign-table scans would mean polling inside a single
`iter-scan` call with no way to yield back to Postgres — the same "scope down
to sync-only" call made for `fivetran-anakin` and `looker-studio-anakin` this
session, for the same underlying reason.

## Project layout

```
Cargo.toml                 crate-type = ["cdylib"], cargo-component metadata
.cargo/config.toml          target = "wasm32-unknown-unknown"
wit/world.wit                this component's own world (anakin:search-fdw@0.1.0)
supabase-wrappers-wit/        the host-provided WIT, copied verbatim from
                               supabase/wrappers wasm-wrappers/wit/v2
src/lib.rs                   the Guest implementation (AnakinSearchFdw)
```

## Table

`anakin_search` is parameterized by an equality predicate on `prompt` — the
same way other Wasm FDWs require an `id =` qual to address a single-resource
endpoint, since `POST /search` has no "list everything" mode.

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

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo install cargo-component --locked --version 0.21.1
cargo component build --release --target wasm32-unknown-unknown
```

Produces `target/wasm32-unknown-unknown/release/anakin_fdw.wasm`. Verified
building clean (zero warnings) with this exact command as of 2026-07-28 —
see `SUBMIT.md`'s "Compile-check status" for the full log and the toolchain
note below.

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

create server anakin_search_server
  foreign data wrapper wasm_wrapper
  options (
    fdw_package_url 'https://github.com/<org>/<repo>/releases/download/v0.1.0/anakin_fdw.wasm',
    fdw_package_name 'anakin:search-fdw',
    fdw_package_version '0.1.0',
    fdw_package_checksum '<sha256 of the released .wasm, printed by the release workflow>',
    api_url 'https://api.anakin.io/v1',  -- optional, this is the default
    api_key_id '<key_id from vault.create_secret above>'
  );

create foreign table anakin_search (
  prompt text,
  search_id text,
  url text,
  title text,
  snippet text,
  date text,
  last_updated text
)
server anakin_search_server
options (
  rowid_column 'url'
);
```

Get a free API key at [anakin.io/dashboard](https://anakin.io/dashboard) —
300 credits, no card required. `search` costs 3 credits per call.

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
  get it to build.

Not verified: an actual `POST /search` round trip against the live Anakin
API, or a `select` against a real foreign table in Postgres. See
`SUBMIT.md` for the full compile log, the toolchain issue that was hit and
fixed along the way, and what's left for a human with Postgres + API key
access.
