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
- End-to-end wall time: ~5m 36s

```json
{
  "success_rate": 0.72,
  "avg_recall": 0.5048,
  "avg_precision": 0.4980,
  "avg_f1": 0.4958
}
```

For comparison the `response_format=html` variant (raw HTML) on the same set
scored `0.547 / 0.407` — markdown extraction wins clearly because the F1
analyzer scores against a human-text snippet, so passing back script tags and
boilerplate hurts precision.

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
