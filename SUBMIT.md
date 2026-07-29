# Supabase Wrappers (Wasm FDW) — submission instructions

This was previously flagged and deliberately deferred as "unfamiliar stack,
real risk of a shallow/wrong attempt." Built now, with the real interface
confirmed from source before writing any Rust — not guessed.

## Update — 2026-07-29: three more sync-only tables added (v0.1.0 → v0.2.0)

The original `v0.1.0` build (below) deliberately scoped this FDW to
`anakin_search` only, since every other Anakin endpoint known at the time was
submit-then-poll and `routines.wit` has no async primitive a foreign-table
scan can represent. Re-examined against that exact constraint: are there
*any other* Anakin endpoints that are synchronous end-to-end, the same way
`POST /search` is?

Checked `anakin-mcp/src/tools/wire.ts` and `ai-visibility.ts` directly (not
assumed) for every tool's handler:

- `wire_catalog` → `client.wireCatalog(slug)` → a single `GET /wire/catalog`
  or `GET /wire/catalog/{slug}` call, no polling.
- `wire_discover` → `client.wireResolve(q, limit)` → a single
  `GET /wire/resolve` call, no polling.
- `ai_visibility_sources` → `client.aiVisibilitySources()` → a single
  `GET /ai-visibility/sources` call, no polling.
- Confirmed NOT sync-only (so still out of scope): `wire_read_action` /
  `wire_write_action` (`POST /wire/task` + poll `GET /wire/jobs/{id}` via
  `runWireAction`/`pollWireJob`), `ai_visibility_search` (`POST
  /ai-visibility/search` + poll), `wire_login`, `wire_build` — all read
  through `client.ts` line by line to confirm the poll loop is real, not
  assumed from the tool name.

That's three genuinely synchronous read endpoints, so three more foreign
tables were added: `anakin_wire_catalog`, `anakin_wire_discover`,
`anakin_ai_visibility_sources`. The mechanism that makes one Wasm component
able to back four foreign tables — table-level `object` option dispatch via
`ctx.get_options(&OptionsType::Table).require("object")` in `begin_scan` /
`begin_modify` — is not invented here: it's the exact pattern the *real*,
currently-shipping `paddle_fdw` and `shopify_fdw` in
`supabase/wrappers/wasm-wrappers/fdw/` use for their own multi-object
wrappers. `paddle_fdw/src/lib.rs` was fetched and read in full
(`gh api repos/supabase/wrappers/contents/wasm-wrappers/fdw/paddle_fdw/src/lib.rs`)
to confirm this before writing any dispatch code — `routines.wit`'s
`Context` resource has no local-foreign-table-name accessor, only
`get-options`, so `object` genuinely is the only thing a Guest impl can
switch on; this was verified, not assumed to be possible.

**Response shapes were pinned from real sources, not guessed:**

- `anakin_wire_catalog` / `anakin_wire_discover`: `anakin-mcp/openapi.yaml`
  ("Anakin Wire API") has full schemas for `/catalog` (`CatalogEntry`) and
  `/resolve` (`ResolveResult`) — used verbatim, field-for-field, the same
  rigor the original `SearchResult`/`SearchResultItem` mapping used against
  `anakin-py`'s Pydantic models.
- `GET /wire/catalog/{slug}` (per-catalog action detail) was deliberately
  **not** added as a table: its response (`{ catalog, actions,
  login_input_schema }`) is a compound object, not a homogeneous list like
  `GET /wire/catalog`'s no-slug mode, so it doesn't decompose into one
  table's column set without inventing a shape the API doesn't return. Only
  the list mode (`GET /wire/catalog`, no slug) is exposed.
- `anakin_ai_visibility_sources`: no OpenAPI schema exists for this endpoint
  anywhere in this codebase. `anakin-tap-anakin/tap_anakin/streams.py`'s
  `AiVisibilitySourcesStream` is the only other place in this codebase that
  has already modeled this response — its own module docstring says field
  names not independently confirmed against a live response get a `raw`
  passthrough alongside best-effort typed fields, rather than a guessed hard
  schema. This FDW's `anakin_ai_visibility_sources` table (`slug`, `label`,
  `raw`) carries that exact same honesty forward instead of inventing false
  certainty: `slug` falls back to `id`, `label` falls back to `name`, and
  `raw` is the full source object verbatim so nothing is lost if the
  best-effort names are wrong.

**Nested/variable fields are `json` columns, not flattened guesses.**
`ResolveResult.params` (`{ required: [...], optional: [...] }`) and
`CatalogEntry.auth_types`/`supported_sources` (string arrays) are exposed as
`json` cells (`Cell::Json`, WIT's `json(string)` variant) rather than invented
flat columns — Postgres gets them as `jsonb`.

**Compile re-verified after the change** — see the fresh log at the bottom of
"Compile-check status" below; same toolchain pin (`cargo-component 0.21.1`,
`rustc 1.95.0`, `wasm32-unknown-unknown`) as the original `v0.1.0` build, no
new dependencies, no `#[allow(warnings)]` added outside the pre-existing
`mod bindings` line.

The WIT component package was renamed `anakin:search-fdw@0.1.0` →
`anakin:anakin-fdw@0.2.0` (and the Rust struct `AnakinSearchFdw` →
`AnakinFdw`) since "search" is no longer an accurate name for a four-table
wrapper — safe to rename now because, per "Not done" below, no
`v0.1.0` tag/release had actually been cut yet (nothing external references
the old package name).

## What was confirmed, from where

All pulled live via `gh api` / `curl` against `raw.githubusercontent.com`,
not assumed or reconstructed from memory:

- **`supabase/wrappers`** (`wasm-wrappers/` subdir) is the monorepo home of
  Supabase's own Wasm FDW examples and the WIT interface definitions.
- **The mechanism is confirmed to be the WebAssembly Component Model**, not a
  plain Rust trait: `wasm-wrappers/wit/v2/world.wit` defines
  `package supabase:wrappers@0.2.0`, world `wrappers`, which imports host
  interfaces (`http`, `jwt`, `stats`, `time`, `types`, `utils`) and
  **exports** `routines` — the interface a guest component must implement.
  Building it is `cargo component build` (the `cargo-component` subcommand
  from bytecodealliance, which wraps `wit-bindgen` codegen + component
  encoding), never plain `cargo build`.
- **The exact `Guest` trait comes from `wit-bindgen`-generated bindings over
  `wasm-wrappers/wit/v2/routines.wit`**, quoted here verbatim (fetched
  2026-07-28):
  ```wit
  host-version-requirement: func() -> string;
  init: func(ctx: borrow<context>) -> fdw-result;
  begin-scan: func(ctx: borrow<context>) -> fdw-result;
  iter-scan: func(ctx: borrow<context>, row: borrow<row>) -> result<option<u32>, fdw-error>;
  re-scan: func(ctx: borrow<context>) -> fdw-result;
  end-scan: func(ctx: borrow<context>) -> fdw-result;
  begin-modify: func(ctx: borrow<context>) -> fdw-result;
  insert: func(ctx: borrow<context>, row: borrow<row>) -> fdw-result;
  update: func(ctx: borrow<context>, rowid: cell, new-row: borrow<row>) -> fdw-result;
  delete: func(ctx: borrow<context>, rowid: cell) -> fdw-result;
  end-modify: func(ctx: borrow<context>) -> fdw-result;
  import-foreign-schema: func(ctx: borrow<context>, stmt: import-foreign-schema-stmt) -> result<list<string>, fdw-error>;
  ```
  every one of these 12 methods is implemented on `AnakinSearchFdw` in
  `src/lib.rs`, matching this signature list one-for-one.
- **Two WIT versions coexist in the repo**: `wit/v1` (`supabase:wrappers@0.1.0`,
  no `import-foreign-schema`, no `uuid`/`other` cell variants) and `wit/v2`
  (`@0.2.0`, has both). The public quickstart template repo,
  [`supabase-community/postgres-wasm-fdw`](https://github.com/supabase-community/postgres-wasm-fdw)
  (linked from the official
  [`docs/guides/create-wasm-wrapper.md`](https://github.com/supabase/wrappers/blob/main/docs/guides/create-wasm-wrapper.md)),
  still targets **v1** with `wit-bindgen-rt = "0.26.0"`. But the FDWs
  actually shipping in the monorepo today — `helloworld_fdw` and `orb_fdw`,
  both read in full before writing anything here — target **v2** with
  `wit-bindgen-rt = "0.41.0"`. This build targets **v2**, matching the
  currently-live examples, not the (apparently stale) public template. A
  real submitter forking the template today would hit this same mismatch
  and should reconcile it before release — flagging it here rather than
  silently picking one.
- **`orb_fdw`** (`wasm-wrappers/fdw/orb_fdw/src/lib.rs`, read in full) was
  the structural template for API-key auth + HTTP JSON calls: server-options
  pattern (`api_key` inline or `api_key_id` → `utils::get_vault_secret`),
  building an `http::Request`, `http::error_for_status`, `stats::inc_stats`
  calls, and the `ctx.get_options(&OptionsType::Server)` (by-reference) call
  style — this differs from the older public template's
  `ctx.get_options(OptionsType::Table)` (by value), another v1-vs-v2
  binding-generation difference. `helloworld_fdw` was the template for the
  minimal scan lifecycle and `import_foreign_schema` shape.
- **Build target is `wasm32-unknown-unknown`**, not `wasm32-wasip1` —
  confirmed from `wasm-wrappers/.cargo/config.toml`,
  `helloworld_fdw/.cargo/config.toml`, and the real release CI workflow
  (`supabase-community/postgres-wasm-fdw/.github/workflows/release_wasm_fdw.yml`):
  ```
  rustup target add wasm32-unknown-unknown
  cargo install cargo-component --version 0.13.2
  cargo component build --release --target wasm32-unknown-unknown
  ```
  This is the exact sequence `README.md`'s Build section documents.
- **The ground-truth API surface** — base URL `https://api.anakin.io/v1`,
  `X-API-Key` header, `search()`'s request/response shape — was read
  directly from `anakin-py/src/anakin/client.py` (`Anakin.search`,
  `HttpClient`) and `anakin-py/src/anakin/models.py` (`SearchResult`,
  `SearchResultItem`), not guessed. `search` is the only Anakin endpoint
  that is synchronous end-to-end; every other endpoint (`scrape`, `map`,
  `crawl`, `agentic_search`, `wire`) is submit-then-poll, and `routines.wit`
  has no async/job/yield primitive a `iter-scan` call could use to represent
  polling — so this FDW is deliberately scoped to `/search` only, the same
  reasoning already applied to `fivetran-anakin` and `looker-studio-anakin`
  this session.

## What's here

*(As of the original `v0.1.0` build — search-only. See the
"Update — 2026-07-29" section at the top of this file for the current
`v0.2.0` four-table state; the layout below is otherwise unchanged.)*

```
Cargo.toml                    crate-type=["cdylib"], package.metadata.component
                               pointing at ./wit and ./supabase-wrappers-wit
.cargo/config.toml             target = "wasm32-unknown-unknown"
wit/world.wit                  anakin:search-fdw@0.1.0, imports host
                                interfaces, exports routines@0.2.0
                                (now anakin:anakin-fdw@0.2.0 — see update)
supabase-wrappers-wit/         http.wit, jwt.wit, routines.wit, stats.wit,
                                time.wit, types.wit, utils.wit, world.wit —
                                downloaded verbatim from
                                supabase/wrappers/wasm-wrappers/wit/v2,
                                byte-for-byte, not retyped by hand
src/lib.rs                     AnakinSearchFdw: implements all 12 Guest
                                methods against POST /search
                                (now AnakinFdw, 4 tables — see update)
README.md, SUBMIT.md
```

`AnakinSearchFdw` exposed one foreign table, `anakin_search`, parameterized
by a required `prompt =` qual (POST /search has no "list all" mode — same
reason `orb_fdw` requires certain quals to address specific endpoints).
`limit` pushes down to the API's `limit` body param (default 5, matching
`anakin-py`'s own default); `offset` is left to Postgres, same as every
other Wasm FDW that only gets the combined bound from `ctx.get_limit()`.
`begin_modify` returns an explicit "read-only" error — search results are
not writable back to the API. (`AnakinFdw` now implements the same shape for
three more tables — see the update section.)

## Verified, not assumed

- Every one of the 12 `routines.wit` v2 function signatures was copied from
  the fetched WIT source and cross-checked character-by-character against
  `helloworld_fdw` and `orb_fdw`'s actual Rust implementations before being
  used in `src/lib.rs` — no signature was written from memory or inferred.
- `Row::push`, `Context::get_quals`/`get_columns`/`get_limit`/`get_options`,
  `Qual::field`/`operator`/`value`, `Column::name`, `Options::get`/`require`/
  `require_or`, and the `http`/`stats`/`utils` module call shapes are all
  used exactly as they appear in `orb_fdw`'s real, currently-compiling
  source — none of that surface was reverse-engineered from the WIT alone
  (WIT doesn't fully pin down e.g. the `Option<&Cell>` vs `Option<Cell>`
  argument style `wit-bindgen` 0.41 generates; the real example did).
- Response JSON shape (`{"id", "results": [{"url","title","snippet","date",
  "last_updated"}]}`) matches `anakin-py`'s Pydantic models exactly, field
  for field, including which fields are optional.
- `rustup target add wasm32-unknown-unknown` succeeded in this sandbox
  (target was already present). All of the above was written *before* the
  build was attempted, from reading WIT + the two real reference
  implementations only — the compile that followed (see below) confirmed
  every one of these claims by actually building against them, rather than
  supplying new information that contradicted them.

## Compile-check status

**It compiles. Real `cargo component build --release`, real `.wasm` component output.**

The toolchain mismatch flagged when this build was picked back up was real
and is documented below, but it was resolved within the 2-3 targeted attempts
budgeted for it — this is not a fallback-to-manual-review situation.

**The exact blocker, precisely:**

- Sandbox `rustc` is 1.95.0 (`rustc 1.95.0 (59807616e 2026-04-14)`), which
  only knows the target name `wasm32-wasip1` (`rustup target list --installed`
  shows `wasm32-unknown-unknown` and `wasm32-wasip1`, no `wasm32-wasi`).
- `cargo-component` 0.13.2 (from crates.io, released 2024) hardcodes a
  preliminary "ensure this target is installed" step that always requests
  `wasm32-wasi` — the pre-rename target name — regardless of what `--target`
  is passed on the command line or set in `.cargo/config.toml`. This is
  unrelated to which target the crate itself actually compiles for
  (`wasm32-unknown-unknown`, confirmed against the real
  `supabase/wrappers/wasm-wrappers/.cargo/config.toml`); it's an internal
  target-provisioning step cargo-component runs before invoking `cargo build`.
- Exact failure, reproduced from a clean `cargo component build --release
  --target wasm32-unknown-unknown` with cargo-component 0.13.2 installed:
  ```
  Installing wasm32-wasi target
  error: toolchain 'stable-aarch64-apple-darwin' does not support target 'wasm32-wasi'; did you mean 'wasm32-wasip1'?
  error: failed to install the `wasm32-wasi` target
  ```
- **Fix applied**: `cargo install cargo-component --locked --version 0.21.1
  --force` (0.21.1 is the current `max_stable_version` on crates.io as of
  2026-07-28 — checked live via the crates.io API, not assumed). Installed
  cleanly (`cargo-component-component 0.21.1`, ~4m38s to compile from
  source, no network/registry issues). Rerunning the identical build command
  with 0.21.1 succeeded immediately with no other changes to any file in
  this directory.
- **README.md's Build section is corrected accordingly** — it now pins
  `--version 0.21.1` instead of `0.13.2`. `0.13.2` remains documented here as
  the version that reproduces the failure, since a real submitter hitting
  the same rustc-version-vs-cargo-component-version skew should recognize it
  immediately rather than re-diagnose from scratch.

**What the successful build actually produced**, verified directly, not
assumed from exit code alone:

- `cargo component build --release --target wasm32-unknown-unknown` exit
  code 0. Full output:
  ```
    Generating bindings for anakin_fdw (src/bindings.rs)
     Locking 12 packages to latest compatible versions
        Adding wit-bindgen-rt v0.41.0 (available: v0.44.0)
     Compiling zmij v1.0.23
     Compiling serde_core v1.0.229
     Compiling serde_json v1.0.151
     Compiling wit-bindgen-rt v0.41.0
     Compiling itoa v1.0.18
     Compiling memchr v2.8.3
     Compiling anakin_fdw v0.1.0 (.../supabase-anakin)
      Finished `release` profile [optimized] target(s) in 4.96s
      Creating component target/wasm32-unknown-unknown/release/anakin_fdw.wasm
  ```
  Zero warnings, zero errors. `cargo component check` (dev profile) also
  passes clean.
- `src/bindings.rs` was **regenerated from scratch** by this build (from the
  WIT files, via `wit-bindgen` 0.41.0 — confirmed by its own header comment
  `// Generated by 'wit-bindgen' 0.41.0. DO NOT EDIT!` and the embedded
  `component-type:wit-bindgen:0.41.0:anakin:search-fdw@0.1.0` custom
  section) — not hand-written, not left over from a prior partial attempt.
  `src/lib.rs` compiled against this freshly generated `Guest` trait without
  any edits needed, confirming the hand-written implementation's method
  signatures were correct against the real WIT all along.
- The output file, `target/wasm32-unknown-unknown/release/anakin_fdw.wasm`
  (134,739 bytes), is a genuine **Component Model** binary, not a plain core
  Wasm module — confirmed by inspecting the raw header bytes directly:
  `00 61 73 6d 0d 00 01 00`. That's the `\0asm` magic followed by version
  `0x000d` / layer `0x0001`, the Component Model's binary encoding (plain
  core modules encode version `0x00000001` with no layer field). This is an
  independent structural check, not just trusting cargo-component's log
  line.
- SHA-256 of this exact local build artifact:
  `fd9d574a9395647a361a854d405df0a12c87842fbb912c29d1945f01c01bc32a`. This is
  **not** the checksum to put in a real `create server ... fdw_package_checksum`
  statement — that must come from the artifact a real release CI pipeline
  produces and hosts (build environments aren't guaranteed to be bit-for-bit
  reproducible across machines/toolchain patch versions). It's recorded here
  purely as evidence this local build ran to completion and produced a
  concrete, hashable artifact, not that it silently no-op'd.

This means the manual line-by-line WIT/reference-example verification
described below was not needed as a *substitute* for compilation — but it
was already done (before the toolchain was retried) and its conclusions
turned out to be correct: every signature it flagged as matching did in fact
compile against the real generated bindings without modification. It's kept
below as evidence of how the implementation was derived, not as a fallback.

### Re-verified after the 2026-07-29 four-table change

Same command, same toolchain pin, rerun from a clean
`target/wasm32-unknown-unknown/release/` after adding `anakin_wire_catalog`,
`anakin_wire_discover`, `anakin_ai_visibility_sources` and the `object`-based
dispatch:

```
$ cargo component build --release --target wasm32-unknown-unknown
  Generating bindings for anakin_fdw (src/bindings.rs)
   Compiling zmij v1.0.23
   Compiling serde_core v1.0.229
   Compiling serde_json v1.0.151
   Compiling wit-bindgen-rt v0.41.0
   Compiling anakin_fdw v0.2.0 (.../supabase-anakin)
    Finished `release` profile [optimized] target(s) in 4.61s
    Creating component target/wasm32-unknown-unknown/release/anakin_fdw.wasm
```

Zero warnings, zero errors. `cargo component check` (dev profile) also passes
clean. Header bytes re-confirmed as a genuine Component Model binary:
`00 61 73 6d 0d 00 01 00`. The `wit-bindgen`-embedded custom section now
reads `component-type:wit-bindgen:0.41.0:anakin:anakin-fdw@0.2.0:anakin:encoded world`
(package/world renamed from `anakin:search-fdw@0.1.0`/`anakin-search`, per the
update note at the top of this file). SHA-256 of this local build artifact:
`e11bf19d04c95b577fcb2ae5d8e52ac3bb438df60e8dbf8f72bd26981f70f5f5` — again,
evidence of a completed local build, not the checksum for a real
`fdw_package_checksum` (that comes from CI's release artifact).

## Not done / real effort remaining for a human

- **`src/bindings.rs` is a build artifact, checked in only as evidence.**
  `mod bindings;` in `src/lib.rs` is regenerated by `cargo component build`
  from the WIT files on every build — it is not a checked-in file in the
  real upstream examples (`helloworld_fdw`'s repo tree has no
  `src/bindings.rs`) and `target/` has been added to `.gitignore` here.
  This directory will not build with plain `rustc`/`cargo build`; it
  requires `cargo component build` specifically (with cargo-component
  >= ~0.14, see the Compile-check section above for the exact version
  constraint and why).
- **Never run against a live Postgres + Wrappers extension.** No
  `create server`/`create foreign table`/`select` round-trip was executed
  against a real Anakin API key or a real Postgres instance — there's no
  Postgres in this sandbox. The SQL in `README.md` is copied structurally
  from the real `orb.md`/`create-wasm-wrapper.md` catalog docs and adapted,
  not tested. Compiling cleanly and producing a valid Component Model
  binary is strong evidence the Rust/WIT surface is right, but it does not
  prove the `POST /search`, `GET /wire/catalog`, `GET /wire/resolve`, or
  `GET /ai-visibility/sources` request/response handling is correct against
  the live API — that needs a real Anakin API key and a real HTTP round
  trip per table, which this sandbox cannot do (no Postgres + Wrappers host
  to invoke the guest's `http` import from). This is the biggest remaining
  risk for the three new tables specifically: their column mappings are
  grounded in `openapi.yaml` / the sibling `anakin-tap-anakin` precedent, not
  in a captured real response body, unlike `anakin_search`'s mapping (which
  was cross-checked against `anakin-py`'s Pydantic models field-for-field).
- **No release pipeline was created.** A real submission needs its own
  GitHub repo (fork of `postgres-wasm-fdw` or built fresh, as done here),
  a `.github/workflows/release_wasm_fdw.yml` copied from the template
  (pinning cargo-component >= 0.21.1, not the template's likely-stale
  0.13.x/0.14.x pin — check whatever CI image/action version is current at
  fork time), a version tag push, and the resulting `fdw_package_checksum`
  filled into `README.md` from *that* build's output, not the local one
  recorded above — none of that exists yet since it requires an actual
  GitHub repo + release, which needs the account owner.
- **`limit` pushdown behavior at the API's actual upper bound is unverified**
  — `anakin-py` documents a default of 5 but no stated maximum; this FDW
  passes whatever `offset + count` Postgres computes through unmodified
  rather than inventing a cap that isn't in the source.

## Steps (needs the account owner)

The Rust/Wasm side is done and compiles clean. What's left is entirely
release-infrastructure and live-integration work that needs credentials and
hosting this sandbox doesn't have:

1. ~~Create a `supabase-anakin` GitHub repo~~ — done:
   `github.com/Anakin-Inc/supabase-anakin`, pushed to `main`.
2. Add `.github/workflows/release_wasm_fdw.yml` (copy from
   `supabase-community/postgres-wasm-fdw`), pinning `cargo-component` to
   **0.21.1 or newer** (not whatever the template currently pins — check it;
   the template may still reference a version too old to know the
   `wasm32-wasip1` target name, the exact failure mode hit and fixed in this
   sandbox). Not yet added.
3. `git tag v0.2.0 && git push origin v0.2.0` — triggers the build, produces
   `anakin_fdw.wasm` + checksum as a GitHub release. (This sandbox's local
   build already proves the same source compiles clean, including after the
   four-table change; the CI run's checksum is what actually goes in the
   install SQL, since it's the artifact that gets hosted and downloaded.)
4. Fill the `fdw_package_checksum` into `README.md`'s install SQL from the
   release output.
5. Smoke-test against a real Supabase/Postgres instance with the Wrappers
   extension and a real Anakin API key before calling this "done" in
   production terms — `select` round-tripping through a real `POST /search`,
   `GET /wire/catalog`, `GET /wire/resolve`, and `GET /ai-visibility/sources`
   call against all four tables is what was not and could not be verified
   here (no Postgres, no live API key in this sandbox).
