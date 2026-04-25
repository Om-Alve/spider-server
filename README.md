# spider-server

A high-performance HTTP crawl service built with [spider](https://github.com/spider-rs/spider), `axum`, and Tokio.

It exposes a JSON API for running bounded, concurrent crawls and is packaged with Docker for deployment.

## Features

- High-throughput async HTTP server (`axum` + Tokio)
- Crawl endpoint backed by `spider` (HTTP crawling path)
- Built-in server backpressure and safety limits:
  - global HTTP request concurrency limit
  - max in-flight crawl jobs limit
  - request body size limit
  - clamped crawl parameters (depth/pages/timeouts/concurrency/content)
- Optional per-page content extraction with truncation
- Container-friendly deployment via multi-stage Docker build and Docker Compose

## API

### Health check

```bash
curl -s http://localhost:8080/healthz
```

Response:

```json
{"status":"ok"}
```

### Scrape (single page)

`POST /scrape`

Fetches a single target page and returns one `page` object. This endpoint is intended for
single-page extraction and benchmark parity with one-shot scrapers.

Example:

```bash
curl -s -X POST http://localhost:8080/scrape \
  -H 'content-type: application/json' \
  -d '{
    "url": "https://spider.cloud",
    "request_timeout_secs": 10,
    "crawl_timeout_secs": 30,
    "respect_robots_txt": true,
    "include_content": true,
    "max_content_chars": 4000,
    "crawl_mode": "http"
  }'
```

Request fields:

- `url` (required): absolute `http://` or `https://` URL
- `request_timeout_secs` (optional)
- `crawl_timeout_secs` (optional)
- `respect_robots_txt` (optional, default true)
- `include_content` (optional, default true)
- `max_content_chars` (optional)
- `proxies` (optional): list of `http://`, `https://`, `socks5://`, or `socks5h://` proxies
- `anti_bot_profile` (optional): `off`, `basic`, `camoufox_like`, or `camoufox_stealth`
- `user_agent` (optional): override User-Agent
- `referer` (optional): set Referer header
- `redirect_policy` (optional): `loose`, `strict`, `none`
- `redirect_limit` (optional): max redirects
- `crawl_mode` (optional): `http`, `browser`, `auto`
- `browser` (optional): same browser options as `/crawl`
- `response_format` (optional): `html` or `text` (default: `text`)
- `camoufox` (optional): camoufox-style HTTP fingerprint configuration:
  - `user_agents`: list of Firefox UAs to rotate (per-host deterministic)
  - `accept_language`: e.g. `"fr-FR,fr;q=0.8"`
  - `platform`: `"linux"`, `"windows"`, or `"macos"` (sets `x-platform` hint)
  - `extra_headers`: extra/override headers merged on top of camoufox set
  - `referer`: default referer if `referer` is not set
  - `do_not_track`: toggle DNT header (default: `true`)

Response (shape):

```json
{
  "root_url": "https://spider.cloud",
  "scrape_duration_ms": 321,
  "mode_used": "http",
  "page": {
    "url": "https://spider.cloud",
    "final_url": "https://spider.cloud/",
    "status_code": 200,
    "bytes": 42137,
    "links_extracted": 14,
    "error": null,
    "content": "<!doctype html>..."
  }
}
```

### Crawl

`POST /crawl`

Example:

```bash
curl -s -X POST http://localhost:8080/crawl \
  -H 'content-type: application/json' \
  -d '{
    "url": "https://spider.cloud",
    "max_depth": 2,
    "max_pages": 50,
    "crawl_concurrency": 16,
    "request_timeout_secs": 10,
    "crawl_timeout_secs": 30,
    "respect_robots_txt": true,
    "subdomains": false,
    "include_content": false,
    "max_content_chars": 4000,
    "proxies": [
      "http://user:pass@proxy-a.example:8080",
      "socks5://proxy-b.example:1080"
    ],
    "anti_bot_profile": "camoufox_like",
    "user_agent": "Mozilla/5.0 ...",
    "referer": "https://www.google.com/",
    "redirect_policy": "loose",
    "redirect_limit": 10
  }'
```

Request fields:

- `url` (required): absolute `http://` or `https://` URL
- `max_depth` (optional)
- `max_pages` (optional)
- `crawl_concurrency` (optional)
- `request_timeout_secs` (optional)
- `crawl_timeout_secs` (optional)
- `respect_robots_txt` (optional, default true)
- `subdomains` (optional, default false)
- `include_content` (optional, default false)
- `max_content_chars` (optional)
- `proxies` (optional): list of `http://`, `https://`, `socks5://`, or `socks5h://` proxies
- `anti_bot_profile` (optional): `off`, `basic`, or `camoufox_like`
- `user_agent` (optional): override User-Agent
- `referer` (optional): set Referer header
- `redirect_policy` (optional): `loose`, `strict`, `none`
- `redirect_limit` (optional): max redirects

Response (shape):

```json
{
  "root_url": "https://spider.cloud",
  "crawl_duration_ms": 1234,
  "pages_fetched": 20,
  "unique_links_seen": 58,
  "pages": [
    {
      "url": "https://spider.cloud",
      "final_url": "https://spider.cloud/",
      "status_code": 200,
      "bytes": 42137,
      "links_extracted": 14,
      "error": null,
      "content": null
    }
  ]
}
```

If crawler capacity is exhausted, the API returns `429 Too Many Requests`.

### Batch crawl

`POST /crawl/batch`

Runs multiple crawl requests concurrently for higher throughput.

Example:

```bash
curl -s -X POST http://localhost:8080/crawl/batch \
  -H 'content-type: application/json' \
  -d '{
    "requests": [
      {
        "url": "https://www.rust-lang.org",
        "max_depth": 1,
        "max_pages": 40,
        "anti_bot_profile": "basic"
      },
      {
        "url": "https://tokio.rs",
        "max_depth": 1,
        "max_pages": 40,
        "proxies": ["http://proxy.example:8080"],
        "anti_bot_profile": "camoufox_like"
      }
    ]
  }'
```

## Local development

```bash
cargo run
```

Server listens on `0.0.0.0:8080` by default.

## Configuration

Environment variables:

- `HOST` (default: `0.0.0.0`)
- `PORT` (default: `8080`)
- `HTTP_CONCURRENCY_LIMIT` (default: `1024`)
- `MAX_CONCURRENT_CRAWLS` (default: available CPU parallelism)
- `REQUEST_BODY_LIMIT_MB` (default: `2`)
- `DEFAULT_MAX_DEPTH` (default: `2`)
- `MAX_ALLOWED_DEPTH` (default: `6`)
- `DEFAULT_MAX_PAGES` (default: `100`)
- `MAX_ALLOWED_PAGES` (default: `5000`)
- `DEFAULT_CRAWL_CONCURRENCY` (default: `16`)
- `MAX_ALLOWED_CRAWL_CONCURRENCY` (default: `256`)
- `DEFAULT_REQUEST_TIMEOUT_SECS` (default: `10`)
- `MAX_REQUEST_TIMEOUT_SECS` (default: `60`)
- `DEFAULT_CRAWL_TIMEOUT_SECS` (default: `30`)
- `MAX_CRAWL_TIMEOUT_SECS` (default: `300`)
- `DEFAULT_CONTENT_CHARS` (default: `4000`)
- `MAX_CONTENT_CHARS` (default: `100000`)
- `DEFAULT_BATCH_SIZE` (default: `4`)
- `MAX_BATCH_SIZE` (default: `64`)
- `MAX_PROXIES_PER_REQUEST` (default: `128`)
- `PROXY_FILE` (default: `proxy.txt`)
- `PROXY_FILE_MAX_ENTRIES` (default: `5000`)
- `PROXY_FILE_DEFAULT_SCHEME` (default: `http`)

### Automatic proxy rotation from `proxy.txt`

You can drop a `proxy.txt` file in the project root (or set `PROXY_FILE` to another path),
and the server will automatically use those proxies for rotation when a request does not include
its own `proxies` list.

Supported line formats:

- `host:port`
- `username:password@host:port`
- `host:port:username:password`
- full URLs like `http://user:pass@host:port`, `socks5://host:port`

Example:

```txt
gate.smartproxy.com:7000:my-user:my-pass
username:password@us.proxy.example:8080
1.2.3.4:3128
```

Notes:

- The file is loaded at startup.
- If a request explicitly sets `proxies`, that request-specific list takes precedence.
- If the file is missing or empty, behavior is unchanged (no default proxy rotation).

## Docker

Build image:

```bash
docker build -t spider-server:latest .
```

Run container:

```bash
docker run --rm -p 8080:8080 spider-server:latest
```

### Docker Compose

```bash
docker compose up --build -d
```

## Notes on performance

- `spider` performs async crawling internally; tune `crawl_concurrency` and page/depth limits to fit your target.
- Prefer `/crawl/batch` when you have many independent URLs and want the server to schedule them concurrently.
- Use rotating proxy lists with `proxies` + `anti_bot_profile=camoufox_like` on stricter targets.
- Protect upstream sites and your own infra by respecting robots and keeping hard bounds enabled.
- For production, place this service behind a reverse proxy/load balancer and tune process CPU/memory limits.
- `/scrape` reuses one `reqwest::Client` per (UA, proxy set, redirect, profile, host) tuple, so repeated calls to the same target avoid TLS/DNS handshakes and reuse connection pools.
- Link counting and emptiness detection use a streaming HTML scan
  (`lol_html` + a SAX-style token counter) instead of a full DOM parse, which
  trims most of the per-response CPU cost.

## Browser mode and auto fallback

The crawl API now supports explicit mode selection:

- `http`: fast HTTP mode (`scrape_raw`)
- `browser`: browser-rendered mode (`scrape`) when spider is built with `chrome`
- `auto`: starts in HTTP mode and falls back to browser mode when results are weak

New request fields:

- `crawl_mode` (`http | browser | auto`) for both `/crawl` and `/scrape`
- `auto_browser_min_pages` (default: `1`)
- `auto_browser_min_links` (default: `20`)
- `browser` (optional object):
  - `chrome_connection_url`
  - `chrome_intercept`
  - `block_visuals`
  - `block_stylesheets`
  - `block_javascript`
  - `block_analytics`

Example auto mode payload:

```json
{
  "url": "https://example.com",
  "crawl_mode": "auto",
  "auto_browser_min_pages": 2,
  "auto_browser_min_links": 40,
  "anti_bot_profile": "camoufox_like",
  "browser": {
    "chrome_connection_url": "http://127.0.0.1:9222",
    "chrome_intercept": true,
    "block_visuals": true,
    "block_stylesheets": false,
    "block_javascript": false,
    "block_analytics": true
  }
}
```

### Single-page auto fallback

`/scrape` also supports `crawl_mode: "auto"`. It first attempts HTTP mode and falls back to
browser mode when the single-page result is missing, non-2xx, or empty.

### Camoufox profiles

This service ships two profiles for emulating a Camoufox-style Firefox client
in HTTP mode:

- `camoufox_like`: realistic Firefox UA, randomized per-host from a curated
  pool, with a full Firefox header set (Accept, Accept-Language,
  Accept-Encoding, Sec-Fetch-*, Upgrade-Insecure-Requests, DNT, Sec-GPC,
  Priority, Cache-Control, Pragma, TE), and a context-aware default Referer.
- `camoufox_stealth`: everything in `camoufox_like` plus enables spider's
  stealth/fingerprint toggles (effective when built with `chrome`) and the
  configured locale. Used automatically as a hardened HTTP retry before
  falling back to a browser, and as the warm-up profile for domains that
  recently failed in HTTP mode.

You can further tune both profiles with the `camoufox` request field:

```json
{
  "url": "https://example.com",
  "anti_bot_profile": "camoufox_stealth",
  "camoufox": {
    "user_agents": [
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:128.0) Gecko/20100101 Firefox/128.0"
    ],
    "accept_language": "en-US,en;q=0.9",
    "platform": "macos",
    "extra_headers": { "x-real-client": "scraper" },
    "referer": "https://duckduckgo.com/",
    "do_not_track": true
  }
}
```

## Benchmarks with rotating proxies

The benchmark harness supports injecting a proxy list from either a direct URL or a local file.
This is useful for evaluating success/quality under geo/rate-limit pressure.

Supported flags (in `scripts/benchmarks/benchmark.py`):

- `--proxy-list-url` (or env `BENCHMARK_PROXY_LIST_URL`)
- `--proxy-list-file` (or env `BENCHMARK_PROXY_LIST_FILE`)
- `--proxy-limit` (default: `50`)

Example:

```bash
python3 scripts/benchmarks/benchmark.py \
  --tools spider-server,crawl4ai \
  --iterations 3 \
  --timeout 30 \
  --skip-server-start \
  --proxy-list-url "https://proxy.webshare.io/.../download/..." \
  --proxy-limit 40 \
  --output scripts/benchmarks/results.json
```

Notes:

- Proxy list parsing accepts plain `host:port` lines and authenticated forms such as
  `username:password@host:port` or `host:port:username:password`.
- The harness normalizes entries to `http://...` before sending them to `/scrape`.
- For scrape quality scoring, the benchmark now requests `response_format: "html"` explicitly.

If you need full Camoufox runtime integration, run a Camoufox-backed browser endpoint
and provide it through `browser.chrome_connection_url`, or add a dedicated Camoufox
backend service behind this API.
