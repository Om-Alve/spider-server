use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use spider::configuration::RedirectPolicy;
use spider::website::Website;
use tokio::{net::TcpListener, sync::Semaphore, time::Instant};
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{compression::CompressionLayer, limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::info;
use url::Url;

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    crawl_permits: Arc<Semaphore>,
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
        }),
        crawl_permits: Arc::new(Semaphore::new(max_concurrent_crawls)),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
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
    let mut results = Vec::with_capacity(payload.requests.len());
    for item in payload.requests {
        match crawl_once(&state, item).await {
            Ok(response) => results.push(BatchCrawlItem {
                ok: true,
                response: Some(response),
                error: None,
            }),
            Err(err) => results.push(BatchCrawlItem {
                ok: false,
                response: None,
                error: Some(err.message),
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
    let proxies = normalize_proxies(payload.proxies, state.config.max_proxies_per_request)?;

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

    let start = Instant::now();
    // Use scrape_raw so page bodies are retained for API quality comparisons.
    website.scrape_raw().await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let pages = website
        .get_pages()
        .map(|spider_pages| {
            spider_pages
                .iter()
                .map(|page| {
                    let html = include_content.then(|| page.get_html());
                    let content = html
                        .as_ref()
                        .map(|value| value.chars().take(max_content_chars).collect::<String>());

                    let error = page.error_status.as_ref().map(ToString::to_string);

                    CrawledPage {
                        url: page.get_url().to_owned(),
                        final_url: page.get_url_final().to_owned(),
                        status_code: page.status_code.as_u16(),
                        bytes: html.as_ref().map_or_else(
                            || page.get_bytes().map_or(0, |bytes| bytes.len()),
                            |value| value.len(),
                        ),
                        links_extracted: page.page_links.as_ref().map_or(0, |links| links.len()),
                        error,
                        content,
                    }
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
