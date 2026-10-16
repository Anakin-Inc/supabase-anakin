# Anakin Wasm FDW for Supabase

A [Supabase Wrappers](https://supabase.com/docs/guides/database/extensions/wrappers) WebAssembly foreign data wrapper for [Anakin](https://anakin.io), a web data API. It lets you scrape pages, map a site's URLs and run web searches from SQL, and store the results next to the rest of your data.

```sql
-- scrape a page to markdown
select url, status, markdown
from anakin.anakin_scrape
where url = 'https://example.com';

-- web search
select rank, title, url, snippet
from anakin.anakin_search
where prompt = 'postgres foreign data wrappers'
limit 5;
```

## Available versions

| Version | Wasm package URL | Checksum | Required Wrappers version |
| ------- | ---------------- | -------- | ------------------------- |
| 0.3.0 | `https://github.com/Anakin-Inc/supabase-anakin/releases/download/v0.3.0/anakin_fdw.wasm` | see `checksum.txt` on the [release](https://github.com/Anakin-Inc/supabase-anakin/releases/tag/v0.3.0) | >=0.5.0 |

## Preparation

### Enable Wrappers

```sql
create extension if not exists wrappers with schema extensions;
```

### Enable the Wasm foreign data wrapper

```sql
create foreign data wrapper wasm_wrapper
  handler wasm_fdw_handler
  validator wasm_fdw_validator;
```

### Store your API key in Vault

Get a key at [anakin.io/dashboard](https://anakin.io/dashboard). Signing up is free and includes trial credits; no card is needed. `anakin_scrape` also works without a key on Anakin's keyless free tier (a small number of credits), which is enough to try it out.

```sql
select vault.create_secret(
  '<Anakin API key>',  -- starts with ask_
  'anakin',
  'Anakin API key for Wrappers'
);
```

### Connect to Anakin

```sql
create server anakin_server
  foreign data wrapper wasm_wrapper
  options (
    fdw_package_url 'https://github.com/Anakin-Inc/supabase-anakin/releases/download/v0.3.0/anakin_fdw.wasm',
    fdw_package_name 'anakin:anakin-fdw',
    fdw_package_version '0.3.0',
    fdw_package_checksum '<checksum from the release>',
    api_key_name 'anakin'  -- Vault secret name; or api_key_id '<key id>', or api_key '<key>' for local testing
  );
```

Server options:

| Option | Required | Description |
| ------ | -------- | ----------- |
| `api_key_name` / `api_key_id` / `api_key` | no | Vault secret name, Vault secret id, or the key itself. Without a key only `anakin_scrape` works. |
| `api_url` | no | API base URL, default `https://api.anakin.io/v1`. |
| `poll_timeout_secs` | no | How long to wait for asynchronous jobs (`anakin_map`, slow scrapes), default `180`. |

### Create the foreign tables

```sql
create schema if not exists anakin;

import foreign schema anakin
  from server anakin_server
  into anakin;
```

This creates six tables. Use `limit to (anakin_scrape, anakin_search)` to import only some of them.

## Tables

Every table is read-only. Each query makes live API calls and uses Anakin credits, so filter on the required column and use `limit`.

### `anakin_scrape`

Scrapes one or more URLs. Requires `where url = '...'` or `where url in (...)`. One API call per URL.

| Column | Type | Notes |
| ------ | ---- | ----- |
| `url` | text | the requested URL (filter) |
| `final_url` | text | URL after redirects |
| `status` | text | `completed` or `failed` |
| `status_code` | bigint | HTTP status of the page |
| `markdown` | text | page content as markdown |
| `html`, `cleaned_html` | text | raw and cleaned HTML |
| `links`, `images` | jsonb | when requested with the `formats` table option |
| `summary` | text | when requested with `formats` |
| `generated_json` | jsonb | structured data, when requested |
| `country` | text | proxy country (filter, optional) |
| `use_browser` | boolean | render with a headless browser (filter, optional) |
| `cached` | boolean | served from Anakin's cache |
| `duration_ms` | bigint | |
| `created_at`, `completed_at` | timestamptz | |
| `error` | text | error message when `status = 'failed'` |
| `id` | text | Anakin job id |
| `attrs` | jsonb | the full API response |

Table options: `formats` (comma-separated, default `markdown`), `country`, `use_browser`.

```sql
select url, status, left(markdown, 200)
from anakin.anakin_scrape
where url in ('https://example.com', 'https://supabase.com')
  and country = 'gb';
```

### `anakin_map`

Lists the URLs found on a site. Requires `where url = '...'`. Needs an API key.

| Column | Type |
| ------ | ---- |
| `url` | text |
| `link` | text |
| `is_external` | boolean |
| `job_id` | text |

Table options: `limit`, `include_subdomains`, `search` (keyword filter on URLs).

### `anakin_search`

Web search. Requires `where prompt = '...'`. A SQL `limit` is passed to the API (default 5). Needs an API key.

| Column | Type |
| ------ | ---- |
| `prompt` | text |
| `search_id` | text |
| `url` | text |
| `title` | text |
| `snippet` | text |
| `date` | text |
| `last_updated` | text |

### `anakin_wire_catalog`, `anakin_wire_discover`, `anakin_ai_visibility_sources`

Read-only views of Anakin's [Wire](https://anakin.io) action catalog (`anakin_wire_catalog`, optional `scope` filter), action search (`anakin_wire_discover`, requires `where q = '...'`) and the list of AI visibility sources. Column lists are in `import foreign schema` output (`\d anakin.anakin_wire_catalog` in psql).

## Examples

Keep a copy of a page's content in a regular table:

```sql
create table pages as
select url, markdown, now() as fetched_at
from anakin.anakin_scrape
where url = 'https://example.com';
```

Refresh it on a schedule with [pg_cron](https://supabase.com/docs/guides/database/extensions/pg_cron):

```sql
select cron.schedule('refresh-pages', '0 6 * * *', $$
  insert into pages
  select url, markdown, now() from anakin.anakin_scrape where url = 'https://example.com'
$$);
```

Store search results:

```sql
create table research as
select prompt, rank, url, title, snippet
from anakin.anakin_search
where prompt = 'open source vector databases'
limit 10;
```

## Limitations

- Queries block while the API works: an inline scrape can take up to about 90 seconds, and `anakin_map` polls until the job finishes or `poll_timeout_secs` passes. Supabase's default `statement_timeout` may need raising for slow pages (`set statement_timeout = '3min';`).
- Only `=` and `in` filters on the documented columns are sent to the API; other filters are applied by Postgres after the rows come back.
- Results are not cached by the wrapper. Copy them into a table (as above) to avoid paying for the same call twice.

## Build from source

```bash
rustup target add wasm32-unknown-unknown
cargo install cargo-component --locked --version 0.21.1
cargo component build --release --target wasm32-unknown-unknown
# output: target/wasm32-unknown-unknown/release/anakin_fdw.wasm
```

Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds the package and publishes it with its checksum as a GitHub release.

## Support

Issues: [github.com/Anakin-Inc/supabase-anakin/issues](https://github.com/Anakin-Inc/supabase-anakin/issues). Anakin docs: [anakin.io/docs](https://anakin.io/docs).

## License

[Apache License 2.0](./LICENSE)
