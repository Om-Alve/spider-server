use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
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
use lol_html::{element, HtmlRewriter, Settings};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use spider::configuration::RedirectPolicy;
use spider::features::chrome_common::RequestInterceptConfiguration;
use spider::page::Page;
use spider::reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use spider::website::Website;
use tokio::{
    net::TcpListener,
    sync::{RwLock, Semaphore},
    time::Instant,
};
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{compression::CompressionLayer, limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::{info, warn};
use url::Url;

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    crawl_permits: Arc<Semaphore>,
    proxy_rotation_cursor: Arc<AtomicUsize>,
    domain_failure_memory: Arc<RwLock<HashMap<String, i32>>>,
    http_client_cache: Arc<RwLock<HashMap<u64, spider::reqwest::Client>>>,
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
    camoufox: Option<CamoufoxConfig>,
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
    camoufox: Option<CamoufoxConfig>,
}

#[derive(Debug, Serialize)]
struct ScrapeResponse {
    root_url: String,
    scrape_duration_ms: u64,
    mode_used: CrawlMode,
    page: Option<CrawledPage>,
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AntiBotProfile {
    Off,
    Basic,
    CamoufoxLike,
    /// Most aggressive: full Firefox/Camoufox-style header set + stealth toggles.
    CamoufoxStealth,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CamoufoxConfig {
    /// Override Firefox UA pool (one is chosen deterministically per request URL).
    user_agents: Option<Vec<String>>,
    /// Locale, e.g. "en-US,en;q=0.9".
    accept_language: Option<String>,
    /// Override platform sec-ch-ua-platform style hint (Linux/Windows/macOS).
    platform: Option<String>,
    /// Optional extra/override headers merged on top of the camoufox header set.
    extra_headers: Option<HashMap<String, String>>,
    /// Default referer for the request when none is provided.
    referer: Option<String>,
    /// Toggle DNT header (default true).
    do_not_track: Option<bool>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScrapeResponseFormat {
    Html,
    Text,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BrowserModeConfig {
    chrome_connection_url: Option<String>,
    chrome_intercept: Option<bool>,
    block_visuals: Option<bool>,
    block_stylesheets: Option<bool>,
    block_javascript: Option<bool>,
    block_analytics: Option<bool>,
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
        http_client_cache: Arc::new(RwLock::new(HashMap::new())),
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
    if let Some(ref referer) = payload.referer {
        website.with_referer(Some(referer.clone()));
    }
    apply_anti_bot_profile(
        &mut website,
        payload.anti_bot_profile.unwrap_or(AntiBotProfile::Basic),
        &payload.url,
        payload.user_agent.as_deref(),
        payload.referer.as_deref(),
        payload.camoufox.as_ref(),
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
            )],
        });
    }
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let pages = website
        .get_pages()
        .map(|spider_pages| {
            spider_pages
                .iter()
                .map(|page| map_page(page, include_content, max_content_chars, None))
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
        .unwrap_or(ScrapeResponseFormat::Text);
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
    let mut mode_used = crawl_mode.clone();
    let page = match &crawl_mode {
        CrawlMode::Browser => {
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
                payload.camoufox.as_ref(),
            );
            fetch_single_page_browser(browser_website).await
        }
        CrawlMode::Http | CrawlMode::Auto => {
            let mut current_page = None;

            // If this domain repeatedly fails in HTTP mode, jump to browser first.
            if force_browser_for_domain {
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
                    AntiBotProfile::CamoufoxStealth,
                    payload.browser.as_ref(),
                    &CrawlMode::Browser,
                    payload.camoufox.as_ref(),
                );
                current_page = fetch_single_page_browser(warm_browser_website).await;
            }

            // Primary HTTP attempt.
            if current_page.is_none()
                || should_retry_scrape_page(current_page.as_ref(), include_content)
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
                    payload.camoufox.as_ref(),
                );
                let cache_key = http_client_cache_key(
                    payload.user_agent.as_deref(),
                    selected_proxies.as_deref().unwrap_or(&[]),
                    request_timeout_secs,
                    redirect_limit,
                    payload.redirect_policy.as_ref(),
                    &requested_profile,
                    payload.referer.as_deref(),
                    &payload.url,
                );
                current_page = fetch_single_page_http_cached(
                    &http_website,
                    &payload.url,
                    Some((state, cache_key)),
                )
                .await;
            }

            // Retry with a rotated proxy batch when defaults are in use.
            if should_retry_scrape_page(current_page.as_ref(), include_content) {
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
                    payload.camoufox.as_ref(),
                );
                current_page = fetch_single_page_http(&rotated_retry_website, &payload.url).await;
            }

            // Hardened retry with camoufox-like profile.
            if should_retry_scrape_page(current_page.as_ref(), include_content) {
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
                    AntiBotProfile::CamoufoxStealth,
                    payload.browser.as_ref(),
                    &CrawlMode::Http,
                    payload.camoufox.as_ref(),
                );
                current_page = fetch_single_page_http(&hardened_website, &payload.url).await;
            }

            // Final browser fallback for stubborn pages/challenges.
            if should_retry_scrape_page(current_page.as_ref(), include_content) {
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
                    AntiBotProfile::CamoufoxStealth,
                    payload.browser.as_ref(),
                    &CrawlMode::Browser,
                    payload.camoufox.as_ref(),
                );
                if let Some(browser_page) = fetch_single_page_browser(browser_website).await {
                    mode_used = CrawlMode::Browser;
                    current_page = Some(browser_page);
                } else {
                    mode_used = if matches!(crawl_mode, CrawlMode::Auto) {
                        CrawlMode::Auto
                    } else {
                        CrawlMode::Http
                    };
                }
            } else {
                mode_used = CrawlMode::Http;
            }

            current_page
        }
    };
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if let Some(host) = host_key.as_deref() {
        let failed = should_retry_scrape_page(page.as_ref(), include_content);
        update_domain_failure_memory(state, host, failed).await;
    }

    Ok(ScrapeResponse {
        root_url: payload.url,
        scrape_duration_ms: elapsed_ms,
        mode_used,
        page: page.as_ref().map(|value| {
            map_page(
                value,
                include_content,
                max_content_chars,
                Some(&response_format),
            )
        }),
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
    camoufox: Option<&CamoufoxConfig>,
) -> Website {
    let mut website = Website::new(url);
    website
        .with_depth(0)
        .with_limit(1)
        .with_concurrency_limit(Some(1))
        .with_request_timeout(Some(Duration::from_secs(request_timeout_secs)))
        .with_crawl_timeout(Some(Duration::from_secs(crawl_timeout_secs)))
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
    apply_anti_bot_profile(
        &mut website,
        anti_bot_profile,
        url,
        user_agent,
        referer.map(|r| r.as_str()),
        camoufox,
    );
    configure_browser_mode(&mut website, browser, crawl_mode);

    website
}

fn should_retry_scrape_page(page: Option<&Page>, require_content: bool) -> bool {
    let Some(page) = page else {
        return true;
    };

    let status = page.status_code.as_u16();
    if matches!(status, 403 | 407 | 408 | 409 | 410 | 425 | 429 | 451) || status >= 500 {
        return true;
    }

    let html_cow = page.get_html_cow();
    let html_trimmed = html_cow.trim();
    if require_content && html_trimmed.is_empty() {
        return true;
    }

    // Fast cheap length pre-check before paying for challenge detection.
    if require_content && html_trimmed.len() < 512 {
        return true;
    }

    if is_challenge_or_block_page(html_trimmed) {
        return true;
    }

    if require_content {
        // Approximate token density without rendering markdown-style text — this
        // is enough to detect pages that are essentially empty shells while
        // avoiding the cost of running html2text on every successful page.
        let token_count = approximate_visible_token_count(html_trimmed);
        if token_count < 40 {
            // Fall back to a real text render to confirm the page is empty
            // before deciding to retry. This way pages that store text inside
            // unusual containers still pass the check.
            let text = html_to_text(html_trimmed);
            if text.split_whitespace().count() < 40 {
                return true;
            }
        }
    }

    false
}

/// Estimate the number of visible word tokens in an HTML document by
/// streaming over the source and skipping `<script>`/`<style>` blocks. This is
/// orders of magnitude cheaper than running html2text just to know whether a
/// page is probably empty.
fn approximate_visible_token_count(html: &str) -> usize {
    let mut count = 0_usize;
    let mut in_tag = false;
    let mut in_skip_block: Option<&'static str> = None;
    let mut in_word = false;
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(tag) = in_skip_block {
            // Look for closing tag like </script> or </style>
            if b == b'<'
                && i + 1 + tag.len() < bytes.len()
                && bytes[i + 1] == b'/'
                && bytes[i + 2..i + 2 + tag.len()].eq_ignore_ascii_case(tag.as_bytes())
            {
                in_skip_block = None;
                i += 2 + tag.len();
                continue;
            }
            i += 1;
            continue;
        }
        if in_tag {
            if b == b'>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }
        if b == b'<' {
            in_tag = true;
            in_word = false;
            // Detect <script or <style start to enter skip block.
            let rest = &bytes[i + 1..];
            if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case(b"script") {
                in_skip_block = Some("script");
            } else if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"style") {
                in_skip_block = Some("style");
            }
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            in_word = false;
        } else if !in_word {
            in_word = true;
            count += 1;
            if count >= 64 {
                return count;
            }
        }
        i += 1;
    }
    count
}

fn is_challenge_or_block_page(html: &str) -> bool {
    let content = html.to_ascii_lowercase();
    let markers = [
        "cloudflare",
        "captcha",
        "access denied",
        "verify you are a human",
        "attention required",
        "datadome",
        "akamai",
        "bot detection",
        "security check",
        "please enable javascript and cookies",
        "oops!! something went wrong",
    ];
    markers.iter().any(|marker| content.contains(marker))
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
) -> CrawledPage {
    // Borrow the HTML once. `get_html_cow` returns a borrowed slice for the
    // common UTF-8 case so we avoid the per-call allocation done by
    // `get_html()`. The actual byte count comes from the underlying buffer
    // directly, which avoids re-encoding non-UTF-8 content twice.
    let raw_bytes = page.get_bytes().map(<[u8]>::len).unwrap_or(0);
    let html_cow = include_content.then(|| page.get_html_cow());

    let content = html_cow.as_ref().map(|value| {
        let rendered = match response_format.unwrap_or(&ScrapeResponseFormat::Html) {
            ScrapeResponseFormat::Html => take_chars(value, max_content_chars),
            ScrapeResponseFormat::Text => {
                let focused = html_to_main_text(value);
                let chosen = if focused.split_whitespace().count() < 40 {
                    html_to_text(value)
                } else {
                    focused
                };
                take_chars(&chosen, max_content_chars)
            }
        };
        rendered
    });

    let error = page.error_status.as_ref().map(ToString::to_string);
    let links_extracted = page.page_links.as_ref().map_or_else(
        || {
            html_cow
                .as_deref()
                .map(count_links_streaming)
                .unwrap_or_else(|| count_links_streaming(page.get_html_cow().as_ref()))
        },
        |links| links.len(),
    );

    let bytes = html_cow.as_ref().map_or(raw_bytes, |value| value.len());

    CrawledPage {
        url: page.get_url().to_owned(),
        final_url: page.get_url_final().to_owned(),
        status_code: page.status_code.as_u16(),
        bytes,
        links_extracted,
        error,
        content,
    }
}

#[inline]
fn take_chars(value: &str, max_chars: usize) -> String {
    if value.len() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect::<String>()
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
    fetch_single_page_http_cached(website, url, None).await
}

async fn fetch_single_page_http_cached(
    website: &Website,
    url: &str,
    cache: Option<(&AppState, u64)>,
) -> Option<Page> {
    let client = if let Some((state, key)) = cache {
        get_or_build_http_client(state, key, website).await
    } else {
        website.configure_http_client()
    };
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

async fn get_or_build_http_client(
    state: &AppState,
    key: u64,
    website: &Website,
) -> spider::reqwest::Client {
    {
        let cache = state.http_client_cache.read().await;
        if let Some(client) = cache.get(&key) {
            return client.clone();
        }
    }
    let client = website.configure_http_client();
    let mut cache = state.http_client_cache.write().await;
    if let Some(existing) = cache.get(&key) {
        return existing.clone();
    }
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(key, client.clone());
    client
}

/// Fingerprint that identifies a Website's HTTP client configuration. Two
/// requests that share this key can share the same `reqwest::Client` (and its
/// connection pool) across requests, which avoids repeated TLS handshakes and
/// significantly improves throughput on hot benchmark targets.
#[allow(clippy::too_many_arguments)]
fn http_client_cache_key(
    user_agent: Option<&str>,
    proxies: &[String],
    request_timeout_secs: u64,
    redirect_limit: usize,
    redirect_policy: Option<&RedirectPolicyRequest>,
    profile: &AntiBotProfile,
    referer: Option<&str>,
    target_url: &str,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    user_agent.hash(&mut hasher);
    proxies.len().hash(&mut hasher);
    for proxy in proxies {
        proxy.hash(&mut hasher);
    }
    request_timeout_secs.hash(&mut hasher);
    redirect_limit.hash(&mut hasher);
    if let Some(p) = redirect_policy {
        std::mem::discriminant(p).hash(&mut hasher);
    } else {
        0_u8.hash(&mut hasher);
    }
    std::mem::discriminant(profile).hash(&mut hasher);
    referer.hash(&mut hasher);
    // Include the host so each origin gets its own cached client. Some hosts
    // do not survive aggressive connection pooling under proxy rotation.
    if let Ok(parsed) = Url::parse(target_url) {
        if let Some(host) = parsed.host_str() {
            host.hash(&mut hasher);
        }
    }
    hasher.finish()
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
    let Some(main_html) = extract_primary_html_block(html) else {
        return html_to_text(html);
    };
    let focused = html_to_text(&main_html);
    let focused_tokens = focused.split_whitespace().count();

    // Cheap heuristic: if the focused fragment's source HTML covers a large
    // fraction of the document, trust the extraction without rendering the
    // full document a second time.
    if focused_tokens >= 120 && main_html.len() * 10 >= html.len().saturating_mul(4) {
        return focused;
    }

    let full = html_to_text(html);
    let full_tokens = full.split_whitespace().count();
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

/// Count anchors with non-empty `href` using a streaming HTML rewriter.
/// This avoids building a full DOM (which `scraper`/`html5ever` does) and is
/// roughly an order of magnitude faster for typical pages.
fn count_links_streaming(html: &str) -> usize {
    let count = RefCell::new(0_usize);
    let result = {
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![element!("a[href]", |el| {
                    if let Some(href) = el.get_attribute("href") {
                        if !href.trim().is_empty() {
                            *count.borrow_mut() += 1;
                        }
                    }
                    Ok(())
                })],
                ..Settings::default()
            },
            |_: &[u8]| {},
        );
        rewriter.write(html.as_bytes()).and_then(|_| rewriter.end())
    };
    if result.is_err() {
        // Fall back to scraper on malformed input rather than returning 0.
        let parsed = Html::parse_document(html);
        if let Ok(selector) = Selector::parse("a[href]") {
            return parsed.select(&selector).count();
        }
        return 0;
    }
    count.into_inner()
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
            "proxy list exceeds configured maximum: {}",
            max_proxies
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

/// Camoufox-style Firefox UA pool. Each entry is a real, recent Firefox UA
/// across major desktop platforms. We deterministically pick one per request
/// URL so consecutive scrapes of the same target stay consistent (which helps
/// avoid suspicion from servers that fingerprint UA churn) while different
/// targets get different identities.
const CAMOUFOX_UA_POOL: &[&str] = &[
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.6; rv:131.0) Gecko/20100101 Firefox/131.0",
];

fn pick_camoufox_user_agent(url: &str, override_pool: Option<&[String]>) -> String {
    let pool: Vec<&str> = match override_pool {
        Some(list) if !list.is_empty() => list.iter().map(String::as_str).collect(),
        _ => CAMOUFOX_UA_POOL.to_vec(),
    };
    if pool.is_empty() {
        return CAMOUFOX_UA_POOL[0].to_string();
    }
    let host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_string());
    let hash = host.bytes().fold(5381_u64, |acc, b| {
        acc.wrapping_mul(33).wrapping_add(b as u64)
    });
    let idx = (hash as usize) % pool.len();
    pool[idx].to_string()
}

fn camoufox_platform_hint(user_agent: &str, override_value: Option<&str>) -> &'static str {
    if let Some(value) = override_value {
        if value.eq_ignore_ascii_case("windows") {
            return "Windows";
        } else if value.eq_ignore_ascii_case("macos") || value.eq_ignore_ascii_case("mac") {
            return "macOS";
        } else if value.eq_ignore_ascii_case("linux") {
            return "Linux";
        }
    }
    if user_agent.contains("Windows") {
        "Windows"
    } else if user_agent.contains("Mac OS") || user_agent.contains("Macintosh") {
        "macOS"
    } else {
        "Linux"
    }
}

/// Build a Camoufox-style Firefox HTTP header set. These headers mirror what a
/// real, fully configured Firefox sends on a top-level navigation, including
/// Sec-Fetch-* metadata, encoding, language, and anti-tracking hints.
fn build_camoufox_headers(
    target_url: &str,
    user_agent: &str,
    referer: Option<&str>,
    config: Option<&CamoufoxConfig>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();

    let accept = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
    let language = config
        .and_then(|c| c.accept_language.as_deref())
        .unwrap_or("en-US,en;q=0.9");
    let dnt = config.and_then(|c| c.do_not_track).unwrap_or(true);
    let platform = camoufox_platform_hint(user_agent, config.and_then(|c| c.platform.as_deref()));

    let entries: &[(&str, &str)] = &[
        ("accept", accept),
        ("accept-language", language),
        ("accept-encoding", "gzip, deflate, br, zstd"),
        ("upgrade-insecure-requests", "1"),
        ("sec-fetch-dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-site", "none"),
        ("sec-fetch-user", "?1"),
        ("sec-gpc", "1"),
        ("priority", "u=0, i"),
        ("cache-control", "no-cache"),
        ("pragma", "no-cache"),
        ("te", "trailers"),
    ];

    for (name, value) in entries {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    }

    if dnt {
        headers.insert("dnt", HeaderValue::from_static("1"));
    }

    headers.insert("x-platform", HeaderValue::from_static(platform));

    if let Some(referer) = referer {
        if let Ok(value) = HeaderValue::from_str(referer) {
            headers.insert(spider::reqwest::header::REFERER, value);
        }
    } else if let Some(referer) = config
        .and_then(|c| c.referer.as_deref())
        .or(Some(camoufox_default_referer(target_url)))
    {
        if let Ok(value) = HeaderValue::from_str(referer) {
            headers.insert(spider::reqwest::header::REFERER, value);
        }
    }

    if let Some(extras) = config.and_then(|c| c.extra_headers.as_ref()) {
        for (name, value) in extras {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
    }

    headers
}

fn camoufox_default_referer(target_url: &str) -> &'static str {
    let host = Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    if host.ends_with(".cn") {
        "https://www.baidu.com/"
    } else if host.ends_with(".ru") {
        "https://yandex.ru/"
    } else if host.ends_with(".jp") {
        "https://www.yahoo.co.jp/"
    } else {
        "https://www.google.com/"
    }
}

fn apply_anti_bot_profile(
    website: &mut Website,
    profile: AntiBotProfile,
    target_url: &str,
    explicit_user_agent: Option<&str>,
    explicit_referer: Option<&str>,
    camoufox: Option<&CamoufoxConfig>,
) {
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
        AntiBotProfile::CamoufoxLike | AntiBotProfile::CamoufoxStealth => {
            let user_agent = explicit_user_agent
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    pick_camoufox_user_agent(target_url, camoufox.and_then(|c| c.user_agents.as_deref()))
                });

            let headers =
                build_camoufox_headers(target_url, &user_agent, explicit_referer, camoufox);

            website
                .with_modify_headers(true)
                .with_modify_http_client_headers(true)
                .with_user_agent(Some(&user_agent))
                .with_headers(Some(headers));

            // Set a referer field so spider also propagates it via its own logic.
            let referer = explicit_referer
                .map(str::to_string)
                .or_else(|| camoufox.and_then(|c| c.referer.clone()))
                .unwrap_or_else(|| camoufox_default_referer(target_url).to_string());
            website.with_referer(Some(referer));

            if matches!(profile, AntiBotProfile::CamoufoxStealth) {
                // These are no-ops without `chrome` feature, but become useful
                // the moment chrome is enabled.
                website.with_stealth(true).with_fingerprint(true);
                if let Some(locale) = camoufox.and_then(|c| c.accept_language.clone()) {
                    website.with_locale(Some(locale));
                }
            }
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
