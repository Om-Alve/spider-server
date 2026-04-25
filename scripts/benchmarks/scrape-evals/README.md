# scrape-evals integration

This directory holds results and the spider-server engine adapter for the
public [`martynasoxylabs/scrape-evals`](https://github.com/martynasoxylabs/scrape-evals)
benchmark — a 1,000-page human-curated dataset that scores web scraping
engines on:

- **Coverage / Success Rate** — did the engine retrieve substantive content
  for the URL?
- **Quality (F1)** — recall × precision of the captured content vs. the
  human-annotated `truth_text` snippet, evaluated on the best-matching content
  window.

For reference, the published `scrape-evals` README lists e.g.
`Crawl4AI = 58.0% / 0.45`, `Rest (requests) = 50.6% / 0.36`, and
`Firecrawl = 80.9% / 0.68`.

## Latest spider-server result (this repo, default config)

- Dataset: `datasets/1-0-0.csv` (1,000 URLs)
- Settings: `crawl_mode=http`, `anti_bot_profile=camoufox_stealth`,
  `response_format=text` (rendered as markdown by `html2text`),
  `respect_robots_txt=false`, `max_workers=24`
- End-to-end wall time: ~5–6m

```json
{
  "success_rate": 0.717,
  "avg_recall":   0.522,
  "avg_precision":0.517,
  "avg_f1":       0.513
}
```

Note the dataset has a hard ceiling: 135 of the 1,000 rows have empty
`truth_text` AND empty `lie_text`, which the official analyzer hard-codes to
`success=False`. So **0.865** is the maximum achievable coverage on this
dataset, not 1.0. Our 0.717 is therefore ~83% of that ceiling.

For comparison the `response_format=html` variant (raw HTML) on the same set
scored `0.547 / 0.407` — text-with-boilerplate-stripping wins clearly because
the F1 analyzer scores against a human-text snippet, so passing back script
tags and boilerplate hurts precision.

### What changed since the first run

- HTTP body is now reliably decompressed — we used to send
  `Accept-Encoding: gzip, deflate, br, zstd` from the camoufox layer, which
  on some upstreams returned raw compressed bytes that the rest of the
  pipeline could not parse.
- Boilerplate (cookie banners, modal overlays, popup signups, scripts,
  styles, dialogs, sucuri/incapsula stubs) is stripped via a streaming
  `lol_html` pass before text rendering. The keyword list is intentionally
  narrow — broad tokens like `header`, `footer`, `nav`, `share`, `comments`,
  `banner`, `tab` regressed many real article wrappers when tried.
- Strong primary-content extraction is restricted to `<article>`, `<main>`,
  `[role=main]`, and Schema.org `[itemtype*=Article|NewsArticle|BlogPosting]`,
  with a token-density gate. Generic `[class*=content]` matches were dropped
  — they were the cause of cookie-banner extractions being mistaken for the
  main content.
- PDFs are detected by their `%PDF-` magic bytes and rendered to plain text
  via `pdf-extract`. 17 PDFs in the dataset previously scored ~0.0; most now
  score in the 0.9–1.0 band.
- Challenge / block-page detection now also matches Incapsula stubs, Anubis
  Proof-of-Work pages, PerimeterX/PX-CAPTCHA, Sucuri WAF, and the most
  common SPA "JavaScript required" stubs.

## Reproduce

```bash
# 1. Build & run spider-server with high content cap so the eval gets full pages
MAX_CONTENT_CHARS=2000000 \
MAX_CRAWL_TIMEOUT_SECS=120 \
MAX_REQUEST_TIMEOUT_SECS=60 \
MAX_CONCURRENT_CRAWLS=64 \
HTTP_CONCURRENCY_LIMIT=4096 \
RUST_LOG=warn cargo run --release

# 2. Clone scrape-evals and drop in the engine adapter
git clone https://github.com/martynasoxylabs/scrape-evals.git
cp scripts/benchmarks/scrape-evals/spider_server_engine.py \
   scrape-evals/engines/spider_server.py
pip install -r scrape-evals/requirements.txt   # or just typer + pandas + python-dotenv

# 3. Run the full quality suite against this spider-server instance
cd scrape-evals
SPIDER_SERVER_FORMAT=text \
SPIDER_SERVER_PROFILE=camoufox_stealth \
SPIDER_SERVER_TIMEOUT_S=45 \
python3 run_eval.py \
  --scrape_engine spider_server \
  --suite quality \
  --output-dir runs \
  --dataset datasets/1-0-0.csv \
  --max-workers 24 \
  --rerun
```

Environment variables understood by `spider_server_engine.py`:

| variable | default | meaning |
| --- | --- | --- |
| `SPIDER_SERVER_URL` | `http://127.0.0.1:8080` | base URL of the spider-server |
| `SPIDER_SERVER_PROFILE` | `camoufox_stealth` | `off / basic / camoufox_like / camoufox_stealth` |
| `SPIDER_SERVER_FORMAT` | `html` | `html` or `text` (text is reported as `markdown` to the analyzer for clean tokenization) |
| `SPIDER_SERVER_MODE` | `http` | `http`, `browser`, or `auto` |
| `SPIDER_SERVER_TIMEOUT_S` | `45` | per-request timeout in seconds |
