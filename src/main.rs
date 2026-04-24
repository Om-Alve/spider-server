use std::{
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
use spider::configuration::RedirectPolicy;
use spider::features::chrome_common::RequestInterceptConfiguration;
use spider::page::Page;
use spider::website::Website;
use tokio::{net::TcpListener, sync::Semaphore, time::Instant};
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{compression::CompressionLayer, limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::{info, warn};
use url::Url;

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    crawl_permits: Arc<Semaphore>,
    proxy_rotation_cursor: Arc<AtomicUsize>,
}

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
            );
            fetch_single_page_browser(browser_website).await
        }
        CrawlMode::Http | CrawlMode::Auto => {
            let mut current_page = {
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
                );
                fetch_single_page_http(&http_website, &payload.url).await
            };

            // Retry with a rotated proxy batch when defaults are in use.
            if should_retry_scrape_page(current_page.as_ref(), include_content)
                && !has_explicit_proxies
                && !state.config.default_proxies.is_empty()
            {
                selected_proxies = resolve_request_proxies(None, state)?;
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
                    AntiBotProfile::CamoufoxLike,
                    payload.browser.as_ref(),
                    &CrawlMode::Http,
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
                    AntiBotProfile::CamoufoxLike,
                    payload.browser.as_ref(),
                    &CrawlMode::Browser,
                );
                if let Some(browser_page) = fetch_single_page_browser(browser_website).await {
                    mode_used = CrawlMode::Browser;
                    current_page = Some(browser_page);
                } else {
                    mode_used = CrawlMode::Http;
                }
            } else {
                mode_used = CrawlMode::Http;
            }

            current_page
        }
    };
    let elapsed_ms = start.elapsed().as_millis() as u64;

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
    apply_anti_bot_profile(&mut website, anti_bot_profile);
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

    let html = page.get_html();
    let html_trimmed = html.trim();
    if require_content && html_trimmed.is_empty() {
        return true;
    }

    if is_challenge_or_block_page(html_trimmed) {
        return true;
    }

    if require_content {
        let text = html_to_text(html_trimmed);
        let token_count = text.split_whitespace().count();
        if html_trimmed.len() < 512 || token_count < 40 {
            return true;
        }
    }

    false
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

fn map_page(
    page: &Page,
    include_content: bool,
    max_content_chars: usize,
    response_format: Option<&ScrapeResponseFormat>,
) -> CrawledPage {
    let html = include_content.then(|| page.get_html());
    let content = html.as_ref().map(|value| {
        let rendered = match response_format.unwrap_or(&ScrapeResponseFormat::Html) {
            ScrapeResponseFormat::Html => value.to_string(),
            ScrapeResponseFormat::Text => html_to_text(value),
        };
        rendered.chars().take(max_content_chars).collect::<String>()
    });

    let error = page.error_status.as_ref().map(ToString::to_string);
    let links_extracted = page.page_links.as_ref().map_or_else(
        || count_links_from_html(&page.get_html()),
        |links| links.len(),
    );

    CrawledPage {
        url: page.get_url().to_owned(),
        final_url: page.get_url_final().to_owned(),
        status_code: page.status_code.as_u16(),
        bytes: html.as_ref().map_or_else(
            || page.get_bytes().map_or(0, |bytes| bytes.len()),
            |value| value.len(),
        ),
        links_extracted,
        error,
        content,
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
