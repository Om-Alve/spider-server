# Benchmark: spider-server (`/scrape`) vs Crawl4AI vs Firecrawl

This directory contains a reproducible benchmark harness for comparing:

- `spider-server` (this repository)
- `crawl4ai`
- `firecrawl` (optional, API key required)

## What is measured

For each target URL and iteration, the harness captures:

- latency (seconds)
- extracted bytes
- extracted link count
- title match quality against a baseline direct HTTP fetch

The spider-server runner uses `POST /scrape` (single-page extraction) instead of
`POST /crawl`, which makes comparisons with Crawl4AI and simple HTTP fetch tools
more apples-to-apples for page-level quality and latency.

Summary metrics include:

- average latency
- p95 latency
- average extracted bytes
- average link count
- title match rate
- content presence rate
- links presence rate

## Targets

Default targets are in `targets.txt`:

- https://www.rust-lang.org
- https://docs.rs
- https://tokio.rs

## Run benchmark

```bash
python3 scripts/benchmarks/benchmark.py --iterations 2 --timeout 60
```

Results are written to:

- `scripts/benchmarks/results.json`

## Requirements

Install benchmark dependencies:

```bash
pip3 install --user crawl4ai firecrawl-py beautifulsoup4
python3 -m playwright install chromium
```

Notes:

- Firecrawl benchmarking requires `FIRECRAWL_API_KEY`.
- In this environment, `firecrawl-py` exposes parse-only functionality without a URL scraping endpoint, so runs may be reported as unavailable unless your installed client/API supports URL crawling.

## Observed run in this environment

From the latest run (`--iterations 2`, 3 targets):

- spider-server: avg latency ~0.119s, p95 ~0.259s, title match rate 1.0
- crawl4ai: avg latency ~0.320s, p95 ~0.468s, title match rate 1.0
- firecrawl: not executed (missing `FIRECRAWL_API_KEY`)

Interpretation:

- For this sample, `spider-server` was faster and extracted more bytes/links.
- Both `spider-server` and Crawl4AI produced correct page titles on all successful runs.
- Firecrawl quality/speed could not be measured without credentials and URL scraping capability.
