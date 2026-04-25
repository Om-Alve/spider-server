# Spider-Server Scraping Requirements For Steiner

This document defines what `spider-server` needs to do for Steiner to fully replace
`crawl4ai` as the scraping and page-to-markdown layer.

The scope is intentionally narrow. Steiner remains the retrieval API: search,
ranking, caching, answer generation, citations, and provider comparison live in
Steiner. `spider-server` should be the fast, reliable extraction service that
turns URLs into useful page content.

## Current Verdict

`spider-server` is already a good fit for Steiner's direction because it moves
browser and crawl complexity out of the Python API process and gives Steiner a
simple HTTP boundary.

The live smoke test was positive:

| URL                                              |     spider-server |            Crawl4AI | Read                                        |
| ------------------------------------------------ | ----------------: | ------------------: | ------------------------------------------- |
| `https://example.com`                            |  158 ms, 15 words |    966 ms, 18 words | both good, spider-server much faster        |
| `https://docs.python.org/3/library/asyncio.html` | 395 ms, 157 words |   945 ms, 519 words | spider-server faster, likely too aggressive |
| `https://github.com/astral-sh/uv`                | 827 ms, 429 words | 4015 ms, 1135 words | spider-server faster and cleaner            |

The replacement is not complete yet. The main gaps are extraction quality
tuning, dynamic-page reliability, response contract stability, and operational
polish.

## Replacement Contract

For Steiner, `spider-server` must reliably provide:

- Fetch one URL and return markdown suitable for LLM context.
- Prefer clean main content over navigation, cookie banners, footers, comments,
  related links, and repository chrome.
- Fall back from fast HTTP mode to browser-rendered mode when static HTML is
  blocked, empty, or clearly incomplete.
- Return enough metadata for Steiner to make caching and fallback decisions.
- Fail explicitly and cheaply when a URL cannot be extracted.
- Maintain predictable latency under concurrent scrape loads.

If `spider-server` satisfies this contract, Steiner can remove Crawl4AI from the
runtime path without losing the useful behavior it depended on.

## Non-Goals

These belong in Steiner, not `spider-server`:

- Search provider integration.
- Reranking.
- Query understanding.
- Multi-source answer generation.
- Citation selection.
- API keys, billing, quotas, and tenant-level product logic.
- Long-term webpage cache policy.
- Retrieval benchmarks against Firecrawl, Exa, Tavily, or other hosted APIs.

`spider-server` can expose telemetry and raw extraction signals, but Steiner
should own product-level retrieval behavior.

## P0: Required To Replace Crawl4AI In Steiner

### 1. Stable Scrape Response Contract

Steiner should not have to infer too much from loosely shaped JSON.

Required response fields:

- `root_url`: requested URL.
- `final_url`: final URL after redirects.
- `status_code`: upstream HTTP status when available.
- `mode_used`: `http`, `browser`, or `auto`.
- `content_type`: upstream content type.
- `duration_ms`: total scrape duration.
- `content`: selected output in the requested format.
- `markdown.raw_markdown`: markdown before content pruning.
- `markdown.fit_markdown`: pruned main-content markdown.
- `markdown.fit_html`: pruned HTML when available.
- `title`: extracted document title.
- `error`: structured error object or `null`.
- `signals`: extraction diagnostics.

Suggested `error` shape:

```json
{
  "code": "timeout | blocked | dns_error | tls_error | http_error | empty_content | browser_error | extract_error",
  "message": "human-readable summary",
  "retryable": true
}
```

Suggested `signals` shape:

```json
{
  "word_count": 1234,
  "char_count": 8901,
  "link_count": 42,
  "content_density": 0.31,
  "main_content_score": 0.82,
  "blocked_likelihood": 0.0,
  "fallback_reason": "http_empty | http_blocked | http_low_density | none"
}
```

Acceptance criteria:

- Steiner can decide `success`, `fallback_to_jina`, and `cacheable` without
  string matching logs.
- Missing markdown is represented as `empty_content`, not as a successful scrape.
- HTTP 4xx/5xx pages are not silently treated as good extraction results.

### 2. Main-Content Extraction Profiles

The current `fit_markdown` behavior can be too aggressive for documentation
pages and still not aggressive enough for some application pages.

Add named extraction profiles:

- `balanced`: default for generic web pages.
- `docs`: preserves headings, tables of contents where useful, API signatures,
  code blocks, admonitions, and nearby explanatory text.
- `article`: prioritizes article body, author/date, headings, and captions.
- `repo`: removes GitHub/GitLab UI chrome while preserving README content,
  install commands, code snippets, and project metadata.
- `minimal`: short, high-signal summary-like content for fast answer context.

Request example:

```json
{
  "url": "https://docs.python.org/3/library/asyncio.html",
  "response_format": "markdown",
  "extraction_profile": "docs"
}
```

Acceptance criteria:

- Python docs pages should not collapse from hundreds of useful words to a tiny
  excerpt unless `minimal` is requested.
- GitHub repo pages should preserve README content while removing sign-in,
  notification, file-list, and repository navigation chrome.
- Code blocks should survive in `docs`, `article`, and `repo` profiles.

### 3. Better Auto Mode

`auto` should be based on measurable weakness in HTTP extraction, not only raw
status or emptiness.

HTTP mode should fall back to browser mode when:

- Status is 403, 429, 503, or other configured blocked statuses.
- Content contains known bot-wall phrases.
- Extracted word count is below a configurable threshold.
- Main content score is low.
- Link count or text density indicates a shell page.
- Required selector or expected text is missing when the caller provides one.

Request controls:

```json
{
  "crawl_mode": "auto",
  "auto_browser_min_words": 150,
  "auto_browser_min_main_content_score": 0.45,
  "wait_for_selector": "main",
  "required_text": "Installation"
}
```

Acceptance criteria:

- `auto` does not use browser mode for pages where HTTP extraction is already
  strong.
- `auto` does use browser mode for client-rendered pages that return a shell in
  HTTP mode.
- Response includes `fallback_reason` when browser fallback is used.

### 4. Browser Mode That Is Production-Usable

Browser mode is the main reason Steiner needed Crawl4AI.

Required browser features:

- Configurable page timeout.
- Wait for load state: `domcontentloaded`, `load`, `networkidle`.
- Optional `wait_for_selector`.
- Optional fixed wait after load.
- JavaScript enable/disable switch.
- Resource blocking for images, media, fonts, analytics, trackers, stylesheets.
- Viewport and user-agent controls.
- Redirect handling.
- Screenshot disabled by default.
- Per-request isolation from previous pages.

Acceptance criteria:

- Browser mode works in Docker without requiring a custom external browser setup.
- Browser crashes return `browser_error` and do not poison the server process.
- Concurrent browser scrapes are bounded by explicit limits.

### 5. Build And Deployment Reliability

The upstream Dockerfile currently pinned a Rust version that could not build the
current dependency graph. Steiner had to add a local workaround Dockerfile.

Required fixes:

- Use a Rust version compatible with locked dependencies.
- Keep `Cargo.lock` checked in and reproducible.
- Add CI that builds the Docker image on every push.
- Publish a versioned image.
- Avoid relying on `latest` for production deployment.

Acceptance criteria:

- `docker build .` works from a clean machine.
- `docker compose up --build` works without local Dockerfile patches.
- The image starts and returns `GET /healthz` within a few seconds.

## P1: Required For High-Confidence Production Use

### 6. Batch Scrape Endpoint For Steiner Prefetch

Steiner often needs to scrape many search results at once.

Required behavior:

- Accept a list of independent scrape requests.
- Preserve input order in the response.
- Apply per-host concurrency limits.
- Apply global concurrency limits.
- Return partial successes.
- Include per-URL duration, mode, status, and error.

Acceptance criteria:

- A batch of 10 URLs should not be slower than 10 sequential single scrapes.
- One slow or failed URL should not fail the whole batch.
- Steiner can map every response item back to the original search result.

### 7. PDF And Non-HTML Handling

Steiner currently has a Python PDF path. Long term, the scrape service should
own content-type-specific extraction.

Required content types:

- HTML.
- PDF.
- Plain text.
- Markdown.
- XML/RSS where text extraction is useful.

Acceptance criteria:

- PDF requests return text/markdown with page count metadata.
- Unsupported content types return `unsupported_content_type`.
- Large documents respect size and timeout limits.

### 8. Content Quality Diagnostics

Steiner should be able to detect bad extraction before sending content to an LLM.

Add diagnostics:

- Word count.
- Unique word ratio.
- Boilerplate ratio.
- Link density.
- Heading count.
- Code block count.
- Main-content confidence.
- Truncation flag.
- Language when cheap to detect.

Acceptance criteria:

- Low-quality extraction can trigger Jina fallback or another retry strategy.
- Truncated content is explicit.
- Debugging bad pages does not require manually inspecting server logs.

### 9. Markdown Normalization

Markdown should be stable enough for caching and downstream chunking.

Required normalization:

- Collapse excessive blank lines.
- Remove tracking links where possible.
- Normalize relative links against `final_url` when links are retained.
- Strip base64 images.
- Preserve tables when useful.
- Preserve code fences.
- Preserve heading hierarchy.
- Avoid huge repeated navigation lists.

Acceptance criteria:

- Same page scraped twice should produce materially stable markdown.
- Markdown should be directly chunkable by Steiner without a cleanup pass beyond
  small local safeguards.

### 10. Observability

Scraping failures are hard to diagnose without structured metrics.

Required metrics:

- Requests by mode and status.
- Duration histogram by mode.
- HTTP fallback-to-browser count.
- Error count by code.
- Upstream status distribution.
- Active HTTP scrape count.
- Active browser scrape count.
- Queue wait time.
- Content word-count histogram.

Acceptance criteria:

- Operators can identify whether failures are network, target-site, extraction,
  or browser-runtime problems.
- Metrics can be scraped by Prometheus.

## P2: Nice To Have After Replacement

### 11. Site-Specific Extraction Rules

Some domains matter disproportionately for retrieval quality.

Useful rule targets:

- GitHub and GitLab repos.
- Python, Rust, Kubernetes, MDN, and cloud-provider docs.
- Wikipedia.
- arXiv abstract pages.
- News/article pages.

Rules should be declarative where possible:

```json
{
  "domain": "github.com",
  "profile": "repo",
  "main_selectors": ["article.markdown-body", "#readme"],
  "drop_selectors": ["nav", "header", "[data-testid='breadcrumbs']"]
}
```

Acceptance criteria:

- Rules improve extraction without hardcoding logic across the Rust codebase.
- Rules can be tested with fixture HTML.

### 12. Fixture-Based Regression Suite

Every extractor change should be tested against saved HTML fixtures.

Required fixture categories:

- Clean docs.
- Noisy docs.
- Article with ads.
- GitHub repo.
- Bot wall.
- Client-rendered shell.
- PDF.
- Redirect.
- Empty page.

Acceptance criteria:

- CI can run extraction tests without network access.
- Tests assert both content presence and boilerplate absence.
- Fixture outputs can be reviewed when extraction behavior changes.

### 13. Scrape Benchmark Harness

Steiner needs a repeatable way to decide whether scraping improved.

Benchmark dimensions:

- Success rate.
- Median and p95 latency.
- Extracted word count.
- Boilerplate ratio.
- Main-content recall.
- Content stability across repeated runs.
- Browser fallback rate.

Benchmark sets:

- `docs`: Python docs, Kubernetes docs, MDN, Rust docs.
- `repos`: GitHub/GitLab README pages.
- `articles`: engineering blogs and news pages.
- `hard`: bot walls, JS-heavy pages, rate-limited pages.
- `pdf`: PDFs and arXiv pages.

Acceptance criteria:

- Steiner can compare `spider-server`, old Crawl4AI output, and Jina fallback
  using the same URL set.
- Results are stored as JSON and summarized in markdown.

## Recommended Implementation Order

1. Fix upstream Docker build and publish a stable image.
2. Lock the `/scrape` response contract.
3. Add extraction profiles, starting with `balanced`, `docs`, and `repo`.
4. Improve `auto` fallback signals.
5. Harden browser mode and concurrency limits.
6. Add batch scrape for Steiner advanced search and prefetch.
7. Add diagnostics and Prometheus metrics.
8. Move PDF extraction into spider-server or define a clear boundary if Steiner
   keeps owning PDFs.
9. Add fixture-based regression tests.
10. Add scrape benchmarks that compare against archived Crawl4AI outputs.

## Definition Of Done For Replacing Crawl4AI

Steiner can treat Crawl4AI as fully replaced when all of the following are true:

- `crawl4ai`, Playwright, and Camoufox are not runtime dependencies of Steiner.
- `docker compose up --build` starts Steiner and spider-server from a clean checkout.
- `/v1/scrape-markdown` succeeds through spider-server for representative docs,
  repos, articles, and simple pages.
- `advanced` search can scrape result pages without direct Crawl4AI code.
- Dynamic pages either extract successfully or return structured retryable errors.
- Bad extraction is detectable from response diagnostics.
- Benchmarks show equal or better latency than Crawl4AI and no unacceptable
  content-quality regressions on Steiner's target URL set.

The target is not feature parity with Crawl4AI as a general crawler. The target is
better scraping for Steiner's retrieval pipeline: faster, cleaner, easier to
operate, and explicit about failure.
