mod fit_markdown;
mod scrape_contract;

use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use spider::configuration::{RedirectPolicy, Viewport, WaitForDelay, WaitForIdleNetwork, WaitForSelector};
use spider::features::chrome_common::RequestInterceptConfiguration;
use spider::page::Page;
use spider::website::Website;
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore},
    task::JoinSet,
    time::Instant,
};
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{compression::CompressionLayer, limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::{info, warn};
use url::Url;

use crate::scrape_contract::{FallbackReason, MarkdownSection, ScrapeError, ScrapeSignals};
use fit_markdown::ExtractionProfile;

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    crawl_permits: Arc<Semaphore>,
    proxy_rotation_cursor: Arc<AtomicUsize>,
    domain_failure_memory: Arc<RwLock<HashMap<String, i32>>>,
}

const DOMAIN_FORCE_BROWSER_THRESHOLD: i32 = 3;
const DOMAIN_MEMORY_MAX_SCORE: i32 = 8;

#[derive(Clone)]
struct ServerConfig {
    default_max_depth: usize,
    max_allowed_depth: usize,
    default_max_pages: u32,
    max_allowed_pages: u32,
    default_crawl_concurrency: usize,
    max_allowed_crawl_concurrency: usize,
    default_request_timeout_secs: u64,
    max_request_timeout_secs: u64,
    default_crawl_timeout_secs: u64,
    max_crawl_timeout_secs: u64,
    default_content_chars: usize,
    max_content_chars: usize,
    default_batch_size: usize,
    max_batch_size: usize,
    max_proxies_per_request: usize,
    default_proxies: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CrawlRequest {
    url: String,
    max_depth: Option<usize>,
    max_pages: Option<u32>,
    crawl_concurrency: Option<usize>,
    request_timeout_secs: Option<u64>,
    crawl_timeout_secs: Option<u64>,
    respect_robots_txt: Option<bool>,
    subdomains: Option<bool>,
    include_content: Option<bool>,
    /// When true, each [`CrawledPage`] may include a [`CrawledPage::markdown`] block (heavier).
    include_markdown: Option<bool>,
    /// Pruning for fit markdown; ignored if `include_markdown` is false.
    #[serde(default)]
    fit_markdown: fit_markdown::FitMarkdownOptions,
    max_content_chars: Option<usize>,
    proxies: Option<Vec<String>>,
    anti_bot_profile: Option<AntiBotProfile>,
    user_agent: Option<String>,
    referer: Option<String>,
    redirect_policy: Option<RedirectPolicyRequest>,
    redirect_limit: Option<usize>,
    crawl_mode: Option<CrawlMode>,
    auto_browser_min_pages: Option<usize>,
    auto_browser_min_links: Option<usize>,
    browser: Option<BrowserModeConfig>,
}

#[derive(Debug, Serialize)]
struct CrawlResponse {
    root_url: String,
    crawl_duration_ms: u64,
    pages_fetched: usize,
    unique_links_seen: usize,
    pages: Vec<CrawledPage>,
}

#[derive(Debug, Deserialize)]
struct ScrapeRequest {
    url: String,
    request_timeout_secs: Option<u64>,
    crawl_timeout_secs: Option<u64>,
    respect_robots_txt: Option<bool>,
    include_content: Option<bool>,
    max_content_chars: Option<usize>,
    proxies: Option<Vec<String>>,
    anti_bot_profile: Option<AntiBotProfile>,
    user_agent: Option<String>,
    referer: Option<String>,
    redirect_policy: Option<RedirectPolicyRequest>,
    redirect_limit: Option<usize>,
    crawl_mode: Option<CrawlMode>,
    browser: Option<BrowserModeConfig>,
    response_format: Option<ScrapeResponseFormat>,
    /// Set true to return [`CrawledPage::markdown`] with `raw_markdown` and `fit_markdown` (Crawl4AI-style).
    include_markdown: Option<bool>,
    #[serde(default)]
    fit_markdown: fit_markdown::FitMarkdownOptions,
    /// When `fit_markdown` is left at defaults, selects pruning presets (`balanced`, `docs`, `article`, `repo`, `minimal`).
    extraction_profile: Option<ExtractionProfile>,
    /// Minimum visible word count before HTTP is considered weak in `auto` mode.
    auto_browser_min_words: Option<usize>,
    /// Minimum [`fit_markdown::quick_main_content_score`] before HTTP is considered weak in `auto` mode.
    auto_browser_min_main_content_score: Option<f64>,
    /// Extra HTTP status codes (in addition to defaults) that trigger browser fallback in `auto` mode.
    auto_browser_blocked_statuses: Option<Vec<u16>>,
    /// If set, `auto` mode may fall back to browser when this text is missing from the HTML body.
    required_text: Option<String>,
    /// If set, browser mode waits for this selector; in `auto`, missing selector can trigger browser after HTTP.
    wait_for_selector: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScrapeResponse {
    root_url: String,
    final_url: Option<String>,
    status_code: Option<u16>,
    content_type: String,
    duration_ms: u64,
    mode_used: CrawlMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    markdown: Option<MarkdownSection>,
    title: Option<String>,
    error: Option<ScrapeError>,
    signals: ScrapeSignals,
    /// Legacy nested page object for older clients and benchmarks.
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<CrawledPage>,
}

#[derive(Debug, Deserialize)]
struct BatchScrapeRequest {
    requests: Vec<ScrapeRequest>,
    #[serde(default)]
    global_concurrency: Option<usize>,
    #[serde(default)]
    per_host_concurrency: Option<usize>,
}

#[derive(Debug, Serialize)]
struct BatchScrapeResponse {
    batch_duration_ms: u64,
    results: Vec<BatchScrapeItem>,
}

#[derive(Debug, Serialize)]
struct BatchScrapeItem {
    index: usize,
    ok: bool,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<ScrapeResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchCrawlRequest {
    requests: Vec<CrawlRequest>,
}

#[derive(Debug, Serialize)]
struct BatchCrawlResponse {
    batch_duration_ms: u64,
    results: Vec<BatchCrawlItem>,
}

#[derive(Debug, Serialize)]
struct BatchCrawlItem {
    ok: bool,
    response: Option<CrawlResponse>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CrawledPage {
    url: String,
    final_url: String,
    status_code: u16,
    bytes: usize,
    links_extracted: usize,
    error: Option<String>,
    content: Option<String>,
    /// Crawl4AI-style markdown: full `raw_markdown` + pruned `fit_markdown` + `fit_html`.
    #[serde(skip_serializing_if = "Option::is_none")]
    markdown: Option<fit_markdown::FitMarkdown>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AntiBotProfile {
    Off,
    Basic,
    CamoufoxLike,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RedirectPolicyRequest {
    Loose,
    Strict,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CrawlMode {
    Http,
    Browser,
    Auto,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScrapeResponseFormat {
    Html,
    Text,
    /// HTML→Markdown for LLM use, with a pruned `fit_markdown` like Crawl4AI.
    Markdown,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BrowserModeConfig {
    chrome_connection_url: Option<String>,
    chrome_intercept: Option<bool>,
    block_visuals: Option<bool>,
    block_stylesheets: Option<bool>,
    block_javascript: Option<bool>,
    block_analytics: Option<bool>,
    /// Hard cap for the browser navigation / resource wait phase (seconds).
    page_timeout_secs: Option<u64>,
    /// `domcontentloaded`, `load`, or `networkidle` (best-effort mapping to spider wait hooks).
    wait_for_load: Option<String>,
    /// Optional post-load delay (milliseconds).
    wait_ms_after_load: Option<u64>,
    /// Timeout for [`Self::wait_for_selector`] (seconds).
    wait_for_selector_timeout_secs: Option<u64>,
    /// When false, disables JavaScript in the intercept pipeline.
    javascript_enabled: Option<bool>,
    viewport_width: Option<u32>,
    viewport_height: Option<u32>,
    /// When true, enables spider screenshot capture (off by default).
    screenshot: Option<bool>,
}

impl From<RedirectPolicyRequest> for RedirectPolicy {
    fn from(value: RedirectPolicyRequest) -> Self {
        match value {
            RedirectPolicyRequest::Loose => RedirectPolicy::Loose,
            RedirectPolicyRequest::Strict => RedirectPolicy::Strict,
            RedirectPolicyRequest::None => RedirectPolicy::None,
        }
    }
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "spider_server=info,tower_http=info".into()),
        )
        .init();

    let host = read_env("HOST", "0.0.0.0");
    let port = read_env_parse("PORT", 8080_u16);
    let request_body_limit_mb = read_env_parse("REQUEST_BODY_LIMIT_MB", 2_usize);
    let http_concurrency_limit = read_env_parse("HTTP_CONCURRENCY_LIMIT", 1024_usize);
    let proxy_file = std::env::var("DEFAULT_PROXY_FILE")
        .or_else(|_| std::env::var("PROXY_FILE"))
        .unwrap_or_else(|_| "proxy.txt".to_string());
    let proxy_file_max_entries = read_env_parse("PROXY_FILE_MAX_ENTRIES", 5_000_usize);
    let proxy_file_default_scheme = read_env("PROXY_FILE_DEFAULT_SCHEME", "http");
    let default_proxies = load_proxies_from_file(
        &proxy_file,
        proxy_file_max_entries,
        &proxy_file_default_scheme,
    );
    let max_concurrent_crawls = read_env_parse(
        "MAX_CONCURRENT_CRAWLS",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );

    let state = AppState {
        config: Arc::new(ServerConfig {
            default_max_depth: read_env_parse("DEFAULT_MAX_DEPTH", 2),
            max_allowed_depth: read_env_parse("MAX_ALLOWED_DEPTH", 6),
            default_max_pages: read_env_parse("DEFAULT_MAX_PAGES", 100),
            max_allowed_pages: read_env_parse("MAX_ALLOWED_PAGES", 5000),
            default_crawl_concurrency: read_env_parse("DEFAULT_CRAWL_CONCURRENCY", 16),
            max_allowed_crawl_concurrency: read_env_parse("MAX_ALLOWED_CRAWL_CONCURRENCY", 256),
            default_request_timeout_secs: read_env_parse("DEFAULT_REQUEST_TIMEOUT_SECS", 10),
            max_request_timeout_secs: read_env_parse("MAX_REQUEST_TIMEOUT_SECS", 60),
            default_crawl_timeout_secs: read_env_parse("DEFAULT_CRAWL_TIMEOUT_SECS", 30),
            max_crawl_timeout_secs: read_env_parse("MAX_CRAWL_TIMEOUT_SECS", 300),
            default_content_chars: read_env_parse("DEFAULT_CONTENT_CHARS", 4000),
            max_content_chars: read_env_parse("MAX_CONTENT_CHARS", 100_000),
            default_batch_size: read_env_parse("DEFAULT_BATCH_SIZE", 4),
            max_batch_size: read_env_parse("MAX_BATCH_SIZE", 64),
            max_proxies_per_request: read_env_parse("MAX_PROXIES_PER_REQUEST", 128),
            default_proxies,
        }),
        crawl_permits: Arc::new(Semaphore::new(max_concurrent_crawls)),
        proxy_rotation_cursor: Arc::new(AtomicUsize::new(0)),
        domain_failure_memory: Arc::new(RwLock::new(HashMap::new())),
    };

    if state.config.default_proxies.is_empty() {
        info!("no rotating proxies loaded from {}", proxy_file);
    } else {
        info!(
            "loaded {} rotating proxies from {}",
            state.config.default_proxies.len(),
            proxy_file
        );
    }

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/scrape", post(scrape))
        .route("/scrape/batch", post(scrape_batch))
        .route("/crawl", post(crawl))
        .route("/crawl/batch", post(crawl_batch))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(RequestBodyLimitLayer::new(
                    request_body_limit_mb * 1024 * 1024,
                ))
                .layer(ConcurrencyLimitLayer::new(http_concurrency_limit)),
        );

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    info!("spider-server listening on {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn crawl(
    State(state): State<AppState>,
    Json(payload): Json<CrawlRequest>,
) -> Result<Json<CrawlResponse>, ApiError> {
    let response = crawl_once(&state, payload).await?;
    Ok(Json(response))
}

async fn scrape(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> Result<Json<ScrapeResponse>, ApiError> {
    let response = scrape_once(&state, payload).await?;
    Ok(Json(response))
}

async fn scrape_batch(
    State(state): State<AppState>,
    Json(payload): Json<BatchScrapeRequest>,
) -> Result<Json<BatchScrapeResponse>, ApiError> {
    if payload.requests.is_empty() {
        return Err(ApiError::bad_request(
            "requests must contain at least one item",
        ));
    }
    let max_b = read_env_parse("MAX_SCRAPE_BATCH", 32usize);
    if payload.requests.len() > max_b {
        return Err(ApiError::bad_request(format!(
            "batch size exceeds maximum ({max_b})"
        )));
    }
    let global_c = payload
        .global_concurrency
        .unwrap_or_else(|| read_env_parse("SCRAPE_BATCH_GLOBAL_CONCURRENCY", 16usize))
        .max(1);
    let per_host = payload
        .per_host_concurrency
        .unwrap_or_else(|| read_env_parse("SCRAPE_BATCH_PER_HOST", 4usize))
        .max(1);

    let global = Arc::new(Semaphore::new(global_c));
    let host_locks: Arc<Mutex<HashMap<String, Arc<Semaphore>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let n = payload.requests.len();
    let start = Instant::now();
    let mut join_set = JoinSet::new();
    for (index, item) in payload.requests.into_iter().enumerate() {
        let state = state.clone();
        let global = global.clone();
        let host_locks = host_locks.clone();
        join_set.spawn(async move {
            let host = extract_host_key(&item.url).unwrap_or_default();
            let host_sem = {
                let mut m = host_locks.lock().await;
                m.entry(host)
                    .or_insert_with(|| Arc::new(Semaphore::new(per_host)))
                    .clone()
            };
            let _g = global
                .acquire_owned()
                .await
                .expect("batch global semaphore");
            let _h = host_sem
                .acquire_owned()
                .await
                .expect("batch host semaphore");
            let inner_start = Instant::now();
            let res = scrape_once(&state, item).await;
            let d = inner_start.elapsed().as_millis() as u64;
            (index, d, res)
        });
    }

    let mut slots: Vec<Option<BatchScrapeItem>> = (0..n).map(|_| None).collect();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((index, duration_ms, Ok(response))) => {
                slots[index] = Some(BatchScrapeItem {
                    index,
                    ok: true,
                    duration_ms,
                    response: Some(response),
                    error: None,
                });
            }
            Ok((index, duration_ms, Err(err))) => {
                slots[index] = Some(BatchScrapeItem {
                    index,
                    ok: false,
                    duration_ms,
                    response: None,
                    error: Some(err.message),
                });
            }
            Err(join_err) => {
                return Err(ApiError::bad_request(format!(
                    "batch worker join error: {join_err}"
                )));
            }
        }
    }

    let mut results: Vec<BatchScrapeItem> = slots.into_iter().flatten().collect();
    results.sort_by_key(|x| x.index);
    Ok(Json(BatchScrapeResponse {
        batch_duration_ms: start.elapsed().as_millis() as u64,
        results,
    }))
}

async fn crawl_batch(
    State(state): State<AppState>,
    Json(payload): Json<BatchCrawlRequest>,
) -> Result<Json<BatchCrawlResponse>, ApiError> {
    if payload.requests.is_empty() {
        return Err(ApiError::bad_request(
            "requests must contain at least one item",
        ));
    }

    let batch_size = clamp(
        payload.requests.len(),
        1,
        state
            .config
            .max_batch_size
            .max(state.config.default_batch_size),
    );
    if payload.requests.len() > batch_size {
        return Err(ApiError::bad_request(format!(
            "batch size exceeds configured maximum: {}",
            state.config.max_batch_size
        )));
    }

    let start = Instant::now();
    let mut tasks = Vec::with_capacity(payload.requests.len());
    for item in payload.requests {
        let state = state.clone();
        tasks.push(tokio::spawn(async move { crawl_once(&state, item).await }));
    }

    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(Ok(response)) => results.push(BatchCrawlItem {
                ok: true,
                response: Some(response),
                error: None,
            }),
            Ok(Err(err)) => results.push(BatchCrawlItem {
                ok: false,
                response: None,
                error: Some(err.message),
            }),
            Err(join_err) => results.push(BatchCrawlItem {
                ok: false,
                response: None,
                error: Some(format!("batch worker join error: {join_err}")),
            }),
        }
    }

    Ok(Json(BatchCrawlResponse {
        batch_duration_ms: start.elapsed().as_millis() as u64,
        results,
    }))
}

async fn crawl_once(state: &AppState, payload: CrawlRequest) -> Result<CrawlResponse, ApiError> {
    validate_target_url(&payload.url)?;

    let _permit = state
        .crawl_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::too_many_requests("crawler is saturated, retry later"))?;

    let depth = clamp(
        payload.max_depth.unwrap_or(state.config.default_max_depth),
        0,
        state.config.max_allowed_depth,
    );
    let pages = clamp(
        payload.max_pages.unwrap_or(state.config.default_max_pages),
        1,
        state.config.max_allowed_pages,
    );
    let concurrency = clamp(
        payload
            .crawl_concurrency
            .unwrap_or(state.config.default_crawl_concurrency),
        1,
        state.config.max_allowed_crawl_concurrency,
    );
    let request_timeout_secs = clamp(
        payload
            .request_timeout_secs
            .unwrap_or(state.config.default_request_timeout_secs),
        1,
        state.config.max_request_timeout_secs,
    );
    let crawl_timeout_secs = clamp(
        payload
            .crawl_timeout_secs
            .unwrap_or(state.config.default_crawl_timeout_secs),
        1,
        state.config.max_crawl_timeout_secs,
    );
    let max_content_chars = clamp(
        payload
            .max_content_chars
            .unwrap_or(state.config.default_content_chars),
        1,
        state.config.max_content_chars,
    );
    let include_content = payload.include_content.unwrap_or(false);
    let include_markdown = payload.include_markdown.unwrap_or(false);
    let redirect_limit = payload.redirect_limit.unwrap_or(10);
    let proxies = resolve_request_proxies(payload.proxies, state)?;
    let crawl_mode = payload.crawl_mode.clone().unwrap_or(CrawlMode::Http);
    let auto_min_pages = payload.auto_browser_min_pages.unwrap_or(1);
    let auto_min_links = payload.auto_browser_min_links.unwrap_or(20);

    let mut website = Website::new(&payload.url);
    website
        .with_depth(depth)
        .with_limit(pages)
        .with_concurrency_limit(Some(concurrency))
        .with_request_timeout(Some(Duration::from_secs(request_timeout_secs)))
        .with_crawl_timeout(Some(Duration::from_secs(crawl_timeout_secs)))
        .with_respect_robots_txt(payload.respect_robots_txt.unwrap_or(true))
        .with_subdomains(payload.subdomains.unwrap_or(false))
        .with_redirect_limit(redirect_limit)
        .with_return_page_links(true);

    if let Some(policy) = payload.redirect_policy {
        website.with_redirect_policy(policy.into());
    }
    if let Some(proxy_list) = proxies {
        website.with_proxies(Some(proxy_list));
    }
    if let Some(user_agent) = payload.user_agent.as_deref() {
        website.with_user_agent(Some(user_agent));
    }
    if let Some(referer) = payload.referer {
        website.with_referer(Some(referer));
    }
    apply_anti_bot_profile(
        &mut website,
        payload.anti_bot_profile.unwrap_or(AntiBotProfile::Basic),
    );
    configure_browser_mode(&mut website, payload.browser.as_ref(), &crawl_mode);

    let start = Instant::now();
    execute_crawl_with_auto_fallback(
        &mut website,
        &crawl_mode,
        payload.browser.as_ref(),
        auto_min_pages,
        auto_min_links,
    )
    .await;
    if let Some(recovered_page) =
        recover_single_page_crawl(&website, &crawl_mode, depth, pages).await
    {
        return Ok(CrawlResponse {
            root_url: payload.url,
            crawl_duration_ms: start.elapsed().as_millis() as u64,
            pages_fetched: 1,
            unique_links_seen: website.get_links().len().max(1),
            pages: vec![map_page(
                &recovered_page,
                include_content,
                max_content_chars,
                None,
                include_markdown,
                &payload.fit_markdown,
            )],
        });
    }
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let pages = website
        .get_pages()
        .map(|spider_pages| {
            spider_pages
                .iter()
                .map(|page| {
                    map_page(
                        page,
                        include_content,
                        max_content_chars,
                        None,
                        include_markdown,
                        &payload.fit_markdown,
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(CrawlResponse {
        root_url: payload.url,
        crawl_duration_ms: elapsed_ms,
        pages_fetched: pages.len(),
        unique_links_seen: website.get_links().len(),
        pages,
    })
}

struct AutoBrowserPolicy {
    min_words: usize,
    min_main_score: f64,
    max_link_density: f64,
    blocked_statuses: Vec<u16>,
}

impl AutoBrowserPolicy {
    fn from_request(payload: &ScrapeRequest) -> Self {
        Self {
            min_words: payload.auto_browser_min_words.unwrap_or(150),
            min_main_score: payload.auto_browser_min_main_content_score.unwrap_or(0.45),
            max_link_density: 6.0,
            blocked_statuses: merge_blocked_statuses(payload.auto_browser_blocked_statuses.clone()),
        }
    }
}

fn merge_blocked_statuses(extra: Option<Vec<u16>>) -> Vec<u16> {
    let mut v = vec![
        403_u16, 407, 408, 409, 410, 425, 429, 451, 503,
    ];
    if let Some(e) = extra {
        for x in e {
            if !v.contains(&x) {
                v.push(x);
            }
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

fn status_triggers_http_retry(status: u16) -> bool {
    matches!(
        status,
        403 | 407 | 408 | 409 | 410 | 425 | 429 | 451
    ) || status >= 500
}

/// Coarse retry signal for proxy rotation (keeps legacy ~40 word threshold).
fn http_retry_for_fetch_attempt(page: Option<&Page>, require_content: bool) -> bool {
    let Some(page) = page else {
        return true;
    };
    let status = page.status_code.as_u16();
    if status_triggers_http_retry(status) {
        return true;
    }
    let html = page.get_html();
    let html_trimmed = html.trim();
    if require_content && html_trimmed.is_empty() {
        return true;
    }
    if scrape_contract::html_challenge_markers_hit(html_trimmed) {
        return true;
    }
    if require_content {
        let wc = scrape_contract::fast_visible_word_count(html_trimmed);
        if html_trimmed.len() < 512 || wc < 40 {
            return true;
        }
    }
    false
}

fn link_count_for_page(page: &Page, html: &str) -> usize {
    page
        .page_links
        .as_ref()
        .map(|s| s.len())
        .unwrap_or_else(|| count_links_from_html(html))
}

fn http_auto_fallback_reason(
    page: Option<&Page>,
    require_content: bool,
    policy: &AutoBrowserPolicy,
    wait_sel: Option<&str>,
    req_text: Option<&str>,
) -> Option<FallbackReason> {
    let Some(page) = page else {
        return Some(FallbackReason::HttpEmpty);
    };
    let status = page.status_code.as_u16();
    if !page.status_code.is_success() {
        if status >= 500 || policy.blocked_statuses.contains(&status) {
            return Some(FallbackReason::HttpBadStatus);
        }
        return None;
    }
    let html = page.get_html();
    let html_trimmed = html.trim();
    if require_content && html_trimmed.is_empty() {
        return Some(FallbackReason::HttpEmpty);
    }
    if scrape_contract::html_challenge_markers_hit(html_trimmed) {
        return Some(FallbackReason::HttpChallenge);
    }
    if let Some(t) = req_text {
        if !scrape_contract::html_contains_text_ci(html_trimmed, t) {
            return Some(FallbackReason::RequiredTextMissing);
        }
    }
    if let Some(sel) = wait_sel {
        if !scrape_contract::html_has_selector(html_trimmed, sel) {
            return Some(FallbackReason::WaitForSelectorMissing);
        }
    }
    if require_content {
        let wc = scrape_contract::fast_visible_word_count(html_trimmed);
        if wc < policy.min_words {
            return Some(FallbackReason::HttpLowWords);
        }
        let score = fit_markdown::quick_main_content_score(html_trimmed);
        if score < policy.min_main_score {
            return Some(FallbackReason::HttpLowMainScore);
        }
        let links = link_count_for_page(page, html_trimmed);
        let ld = if wc > 0 {
            links as f64 / wc as f64
        } else {
            f64::INFINITY
        };
        if ld > policy.max_link_density {
            return Some(FallbackReason::HttpLowDensity);
        }
    }
    None
}

fn effective_crawl_mode(requested: &CrawlMode, used_browser: bool) -> CrawlMode {
    match requested {
        CrawlMode::Auto if !used_browser => CrawlMode::Auto,
        CrawlMode::Auto => CrawlMode::Browser,
        other => other.clone(),
    }
}

fn build_scrape_error(
    page: Option<&Page>,
    crawled: Option<&CrawledPage>,
    want_markdown: bool,
    ctype: &str,
    unsupported: bool,
) -> Option<ScrapeError> {
    if unsupported {
        return Some(ScrapeError {
            code: "unsupported_content_type",
            message: format!("content-type {ctype} is not supported for extraction"),
            retryable: false,
        });
    }
    let Some(page) = page else {
        return Some(ScrapeError {
            code: "empty_content",
            message: "no page could be fetched".into(),
            retryable: true,
        });
    };
    if let Some(err) = page.error_status.as_ref() {
        return Some(scrape_contract::scrape_error_for_network_message(err));
    }
    if !page.status_code.is_success() {
        let st = page.status_code.as_u16();
        return Some(ScrapeError {
            code: "http_error",
            message: format!("upstream HTTP {st}"),
            retryable: st >= 500 || st == 429,
        });
    }
    let html_empty = page.get_html().trim().is_empty();
    if html_empty {
        return Some(ScrapeError {
            code: "empty_content",
            message: "empty document body".into(),
            retryable: true,
        });
    }
    if want_markdown && !unsupported {
        let empty_md = crawled
            .and_then(|c| c.markdown.as_ref())
            .map(|m| m.fit_markdown.trim().is_empty() && m.raw_markdown.trim().is_empty())
            .unwrap_or(true);
        if empty_md {
            return Some(ScrapeError {
                code: "empty_content",
                message: "markdown extraction produced no usable text".into(),
                retryable: false,
            });
        }
    }
    None
}

async fn scrape_once(state: &AppState, payload: ScrapeRequest) -> Result<ScrapeResponse, ApiError> {
    validate_target_url(&payload.url)?;

    let _permit = state
        .crawl_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::too_many_requests("crawler is saturated, retry later"))?;

    let request_timeout_secs = clamp(
        payload
            .request_timeout_secs
            .unwrap_or(state.config.default_request_timeout_secs),
        1,
        state.config.max_request_timeout_secs,
    );
    let crawl_timeout_secs = clamp(
        payload
            .crawl_timeout_secs
            .unwrap_or(state.config.default_crawl_timeout_secs),
        1,
        state.config.max_crawl_timeout_secs,
    );
    let max_content_chars = clamp(
        payload
            .max_content_chars
            .unwrap_or(state.config.default_content_chars),
        1,
        state.config.max_content_chars,
    );
    let include_content = payload.include_content.unwrap_or(true);
    let response_format = payload
        .response_format
        .as_ref()
        .copied()
        .unwrap_or(ScrapeResponseFormat::Text);
    let want_markdown = payload.include_markdown == Some(true)
        || matches!(response_format, ScrapeResponseFormat::Markdown);
    let require_substantive = include_content || want_markdown;
    let redirect_limit = payload.redirect_limit.unwrap_or(10);
    let crawl_mode = payload.crawl_mode.clone().unwrap_or(CrawlMode::Http);
    let respect_robots_txt = payload.respect_robots_txt.unwrap_or(true);
    let has_explicit_proxies = payload.proxies.is_some();
    let mut selected_proxies = resolve_request_proxies(payload.proxies.clone(), state)?;
    let requested_profile = payload
        .anti_bot_profile
        .clone()
        .unwrap_or(AntiBotProfile::CamoufoxLike);
    let host_key = extract_host_key(&payload.url);
    let force_browser_for_domain = if let Some(host) = host_key.as_deref() {
        domain_should_force_browser(state, host).await
    } else {
        false
    };

    let start = Instant::now();
    let auto_policy = AutoBrowserPolicy::from_request(&payload);
    let wait_sel = payload.wait_for_selector.as_deref();
    let req_text = payload.required_text.as_deref();
    let mut used_browser = false;
    let mut fallback_reason = FallbackReason::None;

    let page = match &crawl_mode {
        CrawlMode::Browser => {
            used_browser = true;
            let browser_website = build_scrape_website(
                &payload.url,
                request_timeout_secs,
                crawl_timeout_secs,
                respect_robots_txt,
                redirect_limit,
                payload.redirect_policy.as_ref(),
                selected_proxies.clone(),
                payload.user_agent.as_deref(),
                payload.referer.as_ref(),
                requested_profile.clone(),
                payload.browser.as_ref(),
                &CrawlMode::Browser,
                wait_sel,
            );
            fetch_single_page_browser(browser_website).await
        }
        CrawlMode::Http | CrawlMode::Auto => {
            let mut current_page = Option::<Page>::None;

            if force_browser_for_domain {
                fallback_reason = FallbackReason::HttpBlocked;
                let warm_browser_website = build_scrape_website(
                    &payload.url,
                    request_timeout_secs,
                    crawl_timeout_secs,
                    respect_robots_txt,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    selected_proxies.clone(),
                    payload.user_agent.as_deref(),
                    payload.referer.as_ref(),
                    AntiBotProfile::CamoufoxLike,
                    payload.browser.as_ref(),
                    &CrawlMode::Browser,
                    wait_sel,
                );
                current_page = fetch_single_page_browser(warm_browser_website).await;
                used_browser = current_page.is_some();
            }

            if current_page.is_none()
                || http_retry_for_fetch_attempt(current_page.as_ref(), require_substantive)
            {
                let http_website = build_scrape_website(
                    &payload.url,
                    request_timeout_secs,
                    crawl_timeout_secs,
                    respect_robots_txt,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    selected_proxies.clone(),
                    payload.user_agent.as_deref(),
                    payload.referer.as_ref(),
                    requested_profile.clone(),
                    payload.browser.as_ref(),
                    &CrawlMode::Http,
                    None,
                );
                current_page = fetch_single_page_http(&http_website, &payload.url).await;
            }

            if http_retry_for_fetch_attempt(current_page.as_ref(), require_substantive) {
                if !has_explicit_proxies && !state.config.default_proxies.is_empty() {
                    selected_proxies = resolve_request_proxies(None, state)?;
                }
                let rotated_retry_website = build_scrape_website(
                    &payload.url,
                    request_timeout_secs,
                    crawl_timeout_secs,
                    respect_robots_txt,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    selected_proxies.clone(),
                    payload.user_agent.as_deref(),
                    payload.referer.as_ref(),
                    requested_profile.clone(),
                    payload.browser.as_ref(),
                    &CrawlMode::Http,
                    None,
                );
                current_page = fetch_single_page_http(&rotated_retry_website, &payload.url).await;
            }

            if http_retry_for_fetch_attempt(current_page.as_ref(), require_substantive) {
                if !has_explicit_proxies && !state.config.default_proxies.is_empty() {
                    selected_proxies = resolve_request_proxies(None, state)?;
                }
                let hardened_website = build_scrape_website(
                    &payload.url,
                    request_timeout_secs,
                    crawl_timeout_secs,
                    respect_robots_txt,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    selected_proxies.clone(),
                    payload.user_agent.as_deref(),
                    payload.referer.as_ref(),
                    AntiBotProfile::CamoufoxLike,
                    payload.browser.as_ref(),
                    &CrawlMode::Http,
                    None,
                );
                current_page = fetch_single_page_http(&hardened_website, &payload.url).await;
            }

            let need_final_browser = match crawl_mode {
                CrawlMode::Auto => {
                    if let Some(r) = http_auto_fallback_reason(
                        current_page.as_ref(),
                        require_substantive,
                        &auto_policy,
                        wait_sel,
                        req_text,
                    ) {
                        fallback_reason = r;
                        true
                    } else {
                        false
                    }
                }
                CrawlMode::Http => {
                    http_retry_for_fetch_attempt(current_page.as_ref(), require_substantive)
                }
                CrawlMode::Browser => false,
            };

            if need_final_browser {
                if matches!(crawl_mode, CrawlMode::Http) {
                    fallback_reason = if current_page.as_ref().is_some_and(|p| {
                        scrape_contract::html_challenge_markers_hit(&p.get_html())
                    }) {
                        FallbackReason::HttpChallenge
                    } else if current_page
                        .as_ref()
                        .is_some_and(|p| !p.status_code.is_success())
                    {
                        FallbackReason::HttpBadStatus
                    } else {
                        FallbackReason::HttpLowWords
                    };
                }
                let browser_website = build_scrape_website(
                    &payload.url,
                    request_timeout_secs,
                    crawl_timeout_secs,
                    respect_robots_txt,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    selected_proxies.clone(),
                    payload.user_agent.as_deref(),
                    payload.referer.as_ref(),
                    AntiBotProfile::CamoufoxLike,
                    payload.browser.as_ref(),
                    &CrawlMode::Browser,
                    wait_sel,
                );
                if let Some(browser_page) = fetch_single_page_browser(browser_website).await {
                    used_browser = true;
                    current_page = Some(browser_page);
                }
            }

            current_page
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    if let Some(host) = host_key.as_deref() {
        let failed = page
            .as_ref()
            .map(|p| http_retry_for_fetch_attempt(Some(p), require_substantive))
            .unwrap_or(true);
        update_domain_failure_memory(state, host, failed).await;
    }

    let mode_used = effective_crawl_mode(&crawl_mode, used_browser);
    let content_type = page
        .as_ref()
        .map(|p| scrape_contract::content_type_from_headers(p.headers.as_ref()))
        .unwrap_or_default();
    let unsupported = page
        .as_ref()
        .is_some_and(|_| scrape_contract::classify_content_type(&content_type).is_err());

    let fit_opts = fit_markdown::resolve_fit_options(payload.extraction_profile, &payload.fit_markdown);
    let crawled_page = if unsupported {
        None
    } else {
        page.as_ref().map(|value| {
            map_page(
                value,
                include_content,
                max_content_chars,
                Some(&response_format),
                want_markdown,
                &fit_opts,
            )
        })
    };
    let title = page
        .as_ref()
        .and_then(|p| scrape_contract::extract_title_from_html(&p.get_html()));
    let content = crawled_page.as_ref().and_then(|c| c.content.clone());
    let markdown = crawled_page.as_ref().and_then(|c| {
        c.markdown.as_ref().map(|m| MarkdownSection {
            raw_markdown: m.raw_markdown.clone(),
            fit_markdown: m.fit_markdown.clone(),
            fit_html: m.fit_html.clone(),
        })
    });

    let signals = if unsupported {
        ScrapeSignals {
            word_count: 0,
            char_count: 0,
            unique_word_ratio: 0.0,
            boilerplate_ratio: 0.0,
            link_density: 0.0,
            heading_count: 0,
            code_block_count: 0,
            link_count: 0,
            content_density: 0.0,
            main_content_score: 0.0,
            blocked_likelihood: 0.0,
            fallback_reason: FallbackReason::HttpUnsupportedContentType,
            truncated: false,
            language: None,
        }
    } else if let Some(p) = page.as_ref() {
        let html = p.get_html();
        let links = link_count_for_page(p, &html);
        let main = fit_markdown::quick_main_content_score(&html);
        let fb = if used_browser {
            fallback_reason
        } else {
            FallbackReason::None
        };
        scrape_contract::build_signals(
            p,
            &html,
            links,
            main,
            fb,
            p.content_truncated,
        )
    } else {
        ScrapeSignals {
            word_count: 0,
            char_count: 0,
            unique_word_ratio: 0.0,
            boilerplate_ratio: 0.0,
            link_density: 0.0,
            heading_count: 0,
            code_block_count: 0,
            link_count: 0,
            content_density: 0.0,
            main_content_score: 0.0,
            blocked_likelihood: 0.0,
            fallback_reason: FallbackReason::HttpEmpty,
            truncated: false,
            language: None,
        }
    };

    let error = build_scrape_error(
        page.as_ref(),
        crawled_page.as_ref(),
        want_markdown,
        &content_type,
        unsupported,
    );

    Ok(ScrapeResponse {
        root_url: payload.url.clone(),
        final_url: page.as_ref().map(|p| p.get_url_final().to_string()),
        status_code: page.as_ref().map(|p| p.status_code.as_u16()),
        content_type,
        duration_ms: elapsed_ms,
        mode_used,
        content,
        markdown,
        title,
        error,
        signals,
        page: crawled_page,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_scrape_website(
    url: &str,
    request_timeout_secs: u64,
    crawl_timeout_secs: u64,
    respect_robots_txt: bool,
    redirect_limit: usize,
    redirect_policy: Option<&RedirectPolicyRequest>,
    proxies: Option<Vec<String>>,
    user_agent: Option<&str>,
    referer: Option<&String>,
    anti_bot_profile: AntiBotProfile,
    browser: Option<&BrowserModeConfig>,
    crawl_mode: &CrawlMode,
    wait_for_selector: Option<&str>,
) -> Website {
    let (req_t, crawl_t) = effective_browser_timeouts(browser, request_timeout_secs, crawl_timeout_secs);
    let mut website = Website::new(url);
    website
        .with_depth(0)
        .with_limit(1)
        .with_concurrency_limit(Some(1))
        .with_request_timeout(Some(Duration::from_secs(req_t)))
        .with_crawl_timeout(Some(Duration::from_secs(crawl_t)))
        .with_respect_robots_txt(respect_robots_txt)
        .with_subdomains(false)
        .with_redirect_limit(redirect_limit)
        .with_return_page_links(true);

    if let Some(policy) = redirect_policy.cloned() {
        website.with_redirect_policy(policy.into());
    }
    if let Some(proxy_list) = proxies {
        website.with_proxies(Some(proxy_list));
    }
    if let Some(user_agent) = user_agent {
        website.with_user_agent(Some(user_agent));
    }
    if let Some(referer) = referer {
        website.with_referer(Some(referer.clone()));
    }
    apply_anti_bot_profile(&mut website, anti_bot_profile);
    configure_browser_mode(&mut website, browser, crawl_mode);
    apply_browser_wait_options(
        &mut website,
        browser,
        wait_for_selector,
        req_t,
        crawl_t,
    );

    website
}

fn effective_browser_timeouts(
    browser: Option<&BrowserModeConfig>,
    request_timeout_secs: u64,
    crawl_timeout_secs: u64,
) -> (u64, u64) {
    let Some(b) = browser else {
        return (request_timeout_secs, crawl_timeout_secs);
    };
    let req_t = b
        .page_timeout_secs
        .map(|p| p.clamp(1, request_timeout_secs.max(1)))
        .unwrap_or(request_timeout_secs);
    let crawl_t = b
        .page_timeout_secs
        .map(|p| p.clamp(1, crawl_timeout_secs.max(1)))
        .unwrap_or(crawl_timeout_secs);
    (req_t, crawl_t)
}

fn apply_browser_wait_options(
    website: &mut Website,
    browser: Option<&BrowserModeConfig>,
    wait_for_selector: Option<&str>,
    request_timeout_secs: u64,
    crawl_timeout_secs: u64,
) {
    if let Some(sel) = wait_for_selector.filter(|s| !s.is_empty()) {
        let t = browser
            .and_then(|b| b.wait_for_selector_timeout_secs)
            .unwrap_or(30)
            .max(1);
        website.with_wait_for_selector(Some(WaitForSelector::new(
            Some(Duration::from_secs(t)),
            sel.to_string(),
        )));
    }
    let Some(b) = browser else {
        return;
    };
    if let Some(ms) = b.wait_ms_after_load.filter(|v| *v > 0) {
        website.with_wait_for_delay(Some(WaitForDelay::new(Some(Duration::from_millis(ms)))));
    }
    match b.wait_for_load.as_deref().map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "networkidle" => {
            website.with_wait_for_idle_network(Some(WaitForIdleNetwork::new(Some(
                Duration::from_secs(crawl_timeout_secs.max(5)),
            ))));
        }
        Some(ref s) if s == "load" => {
            website.with_wait_for_almost_idle_network0(Some(WaitForIdleNetwork::new(Some(
                Duration::from_secs(request_timeout_secs.max(3)),
            ))));
        }
        _ => {}
    }
    if let (Some(w), Some(h)) = (b.viewport_width, b.viewport_height) {
        website.with_viewport(Some(Viewport::new(w, h)));
    }
    if b.screenshot != Some(true) {
        website.with_screenshot(None);
    }
}

async fn domain_should_force_browser(state: &AppState, host: &str) -> bool {
    let memory = state.domain_failure_memory.read().await;
    memory.get(host).copied().unwrap_or(0) >= DOMAIN_FORCE_BROWSER_THRESHOLD
}

async fn update_domain_failure_memory(state: &AppState, host: &str, failed: bool) {
    let mut memory = state.domain_failure_memory.write().await;
    let score = memory.entry(host.to_string()).or_insert(0);
    if failed {
        *score = (*score + 1).min(DOMAIN_MEMORY_MAX_SCORE);
    } else {
        *score = (*score - 1).max(0);
    }
    if *score == 0 {
        memory.remove(host);
    }
}

fn map_page(
    page: &Page,
    include_content: bool,
    max_content_chars: usize,
    response_format: Option<&ScrapeResponseFormat>,
    want_markdown: bool,
    fit_options: &fit_markdown::FitMarkdownOptions,
) -> CrawledPage {
    let fmt = response_format.unwrap_or(&ScrapeResponseFormat::Html);
    let full_html = page.get_html();
    let markdown = if want_markdown {
        fit_markdown::build_fit_markdown(&full_html, fit_options)
            .ok()
            .map(|mut m| {
                m.raw_markdown = m
                    .raw_markdown
                    .chars()
                    .take(max_content_chars)
                    .collect();
                m.fit_markdown = m
                    .fit_markdown
                    .chars()
                    .take(max_content_chars)
                    .collect();
                m.fit_html = m.fit_html.chars().take(max_content_chars).collect();
                m
            })
    } else {
        None
    };

    let content = if !include_content {
        None
    } else {
        let rendered = match fmt {
            ScrapeResponseFormat::Html => full_html.to_string(),
            ScrapeResponseFormat::Text => {
                let focused = html_to_main_text(&full_html);
                if focused.split_whitespace().count() < 40 {
                    html_to_text(&full_html)
                } else {
                    focused
                }
            }
            ScrapeResponseFormat::Markdown => {
                if let Some(ref m) = markdown {
                    m.fit_markdown.chars().take(max_content_chars).collect()
                } else {
                    html_to_text(&full_html)
                }
            }
        };
        Some(rendered)
    };

    let error = page.error_status.as_ref().map(ToString::to_string);
    let links_extracted = page.page_links.as_ref().map_or_else(
        || count_links_from_html(&full_html),
        |links| links.len(),
    );
    let bytes = if include_content {
        full_html.len()
    } else {
        page.get_bytes().map_or(0, |b| b.len())
    };

    CrawledPage {
        url: page.get_url().to_owned(),
        final_url: page.get_url_final().to_owned(),
        status_code: page.status_code.as_u16(),
        bytes,
        links_extracted,
        error,
        content,
        markdown,
    }
}

fn configure_browser_mode(
    website: &mut Website,
    browser: Option<&BrowserModeConfig>,
    mode: &CrawlMode,
) {
    if !matches!(mode, CrawlMode::Browser | CrawlMode::Auto) {
        return;
    }

    if let Some(config) = browser {
        website.with_chrome_connection(config.chrome_connection_url.clone());

        let mut intercept =
            RequestInterceptConfiguration::new(config.chrome_intercept.unwrap_or(true));
        if let Some(value) = config.block_visuals {
            intercept.block_visuals = value;
        }
        if let Some(value) = config.block_stylesheets {
            intercept.block_stylesheets = value;
        }
        if let Some(value) = config.block_javascript {
            intercept.block_javascript = value;
        }
        if let Some(value) = config.block_analytics {
            intercept.block_analytics = value;
        }
        if let Some(enabled) = config.javascript_enabled {
            intercept.block_javascript = !enabled;
        }
        website.with_chrome_intercept(intercept);
    } else {
        website.with_chrome_intercept(RequestInterceptConfiguration::new(true));
    }
}

async fn run_crawl_mode(website: &mut Website, mode: &CrawlMode) {
    match mode {
        // scrape_raw keeps pages for API responses while remaining HTTP-first.
        CrawlMode::Http => website.scrape_raw().await,
        // crawl() executes browser path when chrome feature is enabled.
        CrawlMode::Browser => website.scrape().await,
        // auto starts in HTTP mode; optional browser fallback handled by caller.
        CrawlMode::Auto => website.scrape_raw().await,
    }
}

async fn execute_crawl_with_auto_fallback(
    website: &mut Website,
    mode: &CrawlMode,
    browser: Option<&BrowserModeConfig>,
    auto_min_pages: usize,
    auto_min_links: usize,
) -> CrawlMode {
    run_crawl_mode(website, mode).await;

    if matches!(mode, CrawlMode::Auto)
        && should_fallback_to_browser(website, auto_min_pages, auto_min_links)
    {
        let mut browser_website = website.clone();
        configure_browser_mode(&mut browser_website, browser, &CrawlMode::Browser);
        run_crawl_mode(&mut browser_website, &CrawlMode::Browser).await;
        *website = browser_website;
        CrawlMode::Browser
    } else if matches!(mode, CrawlMode::Auto) {
        CrawlMode::Http
    } else {
        mode.clone()
    }
}

async fn recover_single_page_crawl(
    website: &Website,
    mode: &CrawlMode,
    requested_depth: usize,
    requested_pages: u32,
) -> Option<Page> {
    let pages_fetched = website.get_pages().map_or(0, Vec::len);
    let needs_single_page_recovery =
        requested_depth == 0 && requested_pages == 1 && pages_fetched == 0;
    if !needs_single_page_recovery {
        return None;
    }

    fetch_single_page_from_mode(website, mode).await
}

async fn fetch_single_page_http(website: &Website, url: &str) -> Option<Page> {
    let client = website.configure_http_client();
    let initial = Page::new_page(url, &client).await;
    if initial.status_code.is_success() && !initial.get_html().trim().is_empty() {
        return Some(initial);
    }

    // Fallback to a bounded crawl when direct one-shot fetch yields an error page.
    let mut retry = website.clone();
    retry.with_depth(1).with_limit(3);
    retry.scrape_raw().await;
    let target = url.to_string();
    let recovered = retry.get_pages().and_then(|pages| {
        pages
            .iter()
            .find(|page| {
                urls_match(page.get_url(), &target) || urls_match(page.get_url_final(), &target)
            })
            .cloned()
            .or_else(|| pages.first().cloned())
    });

    recovered.or(Some(initial))
}

async fn fetch_single_page_from_mode(website: &Website, mode: &CrawlMode) -> Option<Page> {
    match mode {
        CrawlMode::Http => fetch_single_page_http(website, website.get_url()).await,
        CrawlMode::Browser => fetch_single_page_browser(website.clone()).await,
        CrawlMode::Auto => {
            let http_page = fetch_single_page_http(website, website.get_url()).await;
            if should_fallback_to_browser_page(http_page.as_ref(), true) {
                fetch_single_page_browser(website.clone())
                    .await
                    .or(http_page)
            } else {
                http_page
            }
        }
    }
}

async fn fetch_single_page_browser(mut website: Website) -> Option<Page> {
    let mut rx = website.subscribe(8);
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let target_url = website.get_url().to_string();

    let crawl = async move {
        website.fetch_chrome(Some(target_url.as_str())).await;
        website.unsubscribe();
        let _ = done_tx.send(());
    };

    let sub = async {
        let mut page = None;
        loop {
            tokio::select! {
                biased;
                _ = &mut done_rx => break,
                result = rx.recv() => {
                    if let Ok(next_page) = result {
                        page = Some(next_page);
                    } else {
                        break;
                    }
                }
            }
        }
        page
    };

    let (page, _) = tokio::join!(sub, crawl);
    page
}

fn should_fallback_to_browser_page(page: Option<&Page>, require_content: bool) -> bool {
    let Some(page) = page else {
        return true;
    };
    if !page.status_code.is_success() {
        return true;
    }
    require_content && page.get_html().trim().is_empty()
}

fn html_to_text(html: &str) -> String {
    let width = 120;
    html2text::from_read(Cursor::new(html.as_bytes()), width).unwrap_or_else(|_| html.to_string())
}

fn html_to_main_text(html: &str) -> String {
    let main_html = extract_primary_html_block(html).unwrap_or_else(|| html.to_string());
    let focused = html_to_text(&main_html);
    let full = html_to_text(html);

    let focused_tokens = focused.split_whitespace().count();
    let full_tokens = full.split_whitespace().count();

    // Prefer focused extraction only when it still preserves a substantial
    // amount of page text; otherwise keep the fuller text for recall.
    if focused_tokens >= 120
        && (full_tokens == 0 || focused_tokens * 10 >= full_tokens.saturating_mul(4))
    {
        focused
    } else {
        full
    }
}

fn extract_primary_html_block(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let selectors = [
        "article",
        "main",
        "[role=main]",
        "section[id*=content]",
        "div[id*=content]",
        "section[class*=content]",
        "div[class*=content]",
        "section[class*=article]",
        "div[class*=article]",
    ];

    let mut best: Option<(usize, String)> = None;
    for selector_src in selectors {
        let Ok(selector) = Selector::parse(selector_src) else {
            continue;
        };
        for element in doc.select(&selector) {
            let text = element.text().collect::<Vec<_>>().join(" ");
            let token_count = text.split_whitespace().count();
            if token_count < 40 {
                continue;
            }
            let html_fragment = element.html();
            match &best {
                Some((best_tokens, _)) if *best_tokens >= token_count => {}
                _ => best = Some((token_count, html_fragment)),
            }
        }
    }

    best.map(|(_, fragment)| fragment)
}

fn count_links_from_html(html: &str) -> usize {
    let parsed = Html::parse_document(html);
    let Ok(selector) = Selector::parse("a[href]") else {
        return 0;
    };
    parsed.select(&selector).count()
}

fn urls_match(a: &str, b: &str) -> bool {
    a == b || a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn extract_host_key(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
}

fn should_fallback_to_browser(website: &Website, min_pages: usize, min_links: usize) -> bool {
    let pages = website.get_pages().map_or(0, Vec::len);
    let links = website.get_links().len();
    pages < min_pages || links < min_links
}

fn normalize_proxies(
    proxies: Option<Vec<String>>,
    max_proxies: usize,
) -> Result<Option<Vec<String>>, ApiError> {
    let Some(list) = proxies else {
        return Ok(None);
    };

    let mut normalized = Vec::with_capacity(list.len());
    for proxy in list {
        let value = proxy.trim();
        if value.is_empty() {
            continue;
        }
        if !(value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("socks5://")
            || value.starts_with("socks5h://"))
        {
            return Err(ApiError::bad_request(
                "proxy entries must use http://, https://, socks5://, or socks5h://",
            ));
        }
        normalized.push(value.to_string());
    }

    if normalized.len() > max_proxies {
        return Err(ApiError::bad_request(format!(
            "proxy list exceeds configured maximum: {max_proxies}"
        )));
    }

    Ok((!normalized.is_empty()).then_some(normalized))
}

fn resolve_request_proxies(
    request_proxies: Option<Vec<String>>,
    state: &AppState,
) -> Result<Option<Vec<String>>, ApiError> {
    if request_proxies.is_some() {
        return normalize_proxies(request_proxies, state.config.max_proxies_per_request);
    }

    if state.config.default_proxies.is_empty() {
        return Ok(None);
    }

    let defaults = &state.config.default_proxies;
    let count = state.config.max_proxies_per_request.min(defaults.len());
    let offset = state.proxy_rotation_cursor.fetch_add(1, Ordering::Relaxed) % defaults.len();

    let rotated = (0..count)
        .map(|idx| defaults[(offset + idx) % defaults.len()].clone())
        .collect::<Vec<_>>();

    Ok(Some(rotated))
}

fn load_proxies_from_file(path: &str, max_entries: usize, default_scheme: &str) -> Vec<String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("failed reading proxy file '{}': {}", path, err);
            }
            return Vec::new();
        }
    };

    let mut proxies = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        if proxies.len() >= max_entries {
            break;
        }

        match normalize_single_proxy(line, default_scheme) {
            Ok(Some(proxy)) => proxies.push(proxy),
            Ok(None) => {}
            Err(err) => {
                warn!(
                    "ignoring invalid proxy at {}:{} ({})",
                    path,
                    line_idx + 1,
                    err.message
                );
            }
        }
    }

    proxies
}

fn normalize_single_proxy(value: &str, default_scheme: &str) -> Result<Option<String>, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') {
        return Ok(None);
    }

    if value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("socks5://")
        || value.starts_with("socks5h://")
    {
        return Ok(Some(value.to_string()));
    }

    if value.contains("://") {
        return Err(ApiError::bad_request(
            "proxy entries must use http://, https://, socks5://, or socks5h://",
        ));
    }

    let scheme = match default_scheme.trim().to_ascii_lowercase().as_str() {
        "http" => "http",
        "https" => "https",
        "socks5" => "socks5",
        "socks5h" => "socks5h",
        _ => "http",
    };

    if !value.contains('@') && value.split(':').count() == 4 {
        let mut parts = value.splitn(4, ':');
        let host = parts.next().unwrap_or_default();
        let port = parts.next().unwrap_or_default();
        let username = parts.next().unwrap_or_default();
        let password = parts.next().unwrap_or_default();
        if host.is_empty() || port.is_empty() || username.is_empty() || password.is_empty() {
            return Err(ApiError::bad_request(
                "proxy entries must include host and port",
            ));
        }
        return Ok(Some(format!(
            "{scheme}://{username}:{password}@{host}:{port}"
        )));
    }

    if value.contains(':') {
        return Ok(Some(format!("{scheme}://{value}")));
    }

    Err(ApiError::bad_request(
        "proxy entries must include host:port",
    ))
}

fn apply_anti_bot_profile(website: &mut Website, profile: AntiBotProfile) {
    match profile {
        AntiBotProfile::Off => {
            website
                .with_modify_headers(false)
                .with_modify_http_client_headers(false);
        }
        AntiBotProfile::Basic => {
            website
                .with_modify_headers(true)
                .with_modify_http_client_headers(true);
        }
        AntiBotProfile::CamoufoxLike => {
            // HTTP-mode approximation of camoufox-style evasive posture.
            website
                .with_modify_headers(true)
                .with_modify_http_client_headers(true)
                .with_user_agent(Some(
                    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
                ))
                .with_referer(Some("https://www.google.com/".to_string()));
        }
    }
}

fn read_env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn read_env_parse<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<T>().ok())
        .unwrap_or(default)
}

fn validate_target_url(url: &str) -> Result<(), ApiError> {
    let parsed =
        Url::parse(url).map_err(|_| ApiError::bad_request("url must be a valid absolute URL"))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ApiError::bad_request("url must use http or https"));
    }

    Ok(())
}

fn clamp<T>(value: T, min: T, max: T) -> T
where
    T: Ord,
{
    value.max(min).min(max)
}
