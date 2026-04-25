//! Steiner-facing scrape response helpers: signals, errors, content-type, and fast HTML heuristics.

use axum::http::{header::CONTENT_TYPE, HeaderMap};
use scraper::{Html, Selector};
use serde::Serialize;
use spider::page::Page;

#[derive(Debug, Clone, Serialize)]
pub struct ScrapeError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    None,
    HttpEmpty,
    HttpBlocked,
    HttpLowDensity,
    HttpLowWords,
    HttpLowMainScore,
    HttpChallenge,
    HttpBadStatus,
    HttpUnsupportedContentType,
    RequiredTextMissing,
    WaitForSelectorMissing,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScrapeSignals {
    pub word_count: usize,
    pub char_count: usize,
    pub unique_word_ratio: f64,
    pub boilerplate_ratio: f64,
    pub link_density: f64,
    pub heading_count: usize,
    pub code_block_count: usize,
    pub link_count: usize,
    pub content_density: f64,
    pub main_content_score: f64,
    pub blocked_likelihood: f64,
    pub fallback_reason: FallbackReason,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarkdownSection {
    pub raw_markdown: String,
    pub fit_markdown: String,
    pub fit_html: String,
}

pub fn content_type_from_headers(headers: Option<&HeaderMap>) -> String {
    headers
        .and_then(|h| h.get(CONTENT_TYPE))
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

/// Returns `Err(code)` when the response body should not be treated as extractable HTML.
pub fn classify_content_type(ct: &str) -> Result<(), &'static str> {
    if ct.is_empty() {
        return Ok(());
    }
    if ct.starts_with("text/html")
        || ct.starts_with("application/xhtml+xml")
        || ct.starts_with("application/xhtml")
    {
        return Ok(());
    }
    if ct.starts_with("text/plain") {
        return Ok(());
    }
    if ct.starts_with("text/markdown") || ct.starts_with("text/x-markdown") {
        return Ok(());
    }
    if ct.contains("xml") && (ct.starts_with("text/") || ct.starts_with("application/")) {
        return Ok(());
    }
    if ct.starts_with("application/pdf") || ct.starts_with("application/octet-stream") {
        return Err("unsupported_content_type");
    }
    if ct.starts_with("image/")
        || ct.starts_with("video/")
        || ct.starts_with("audio/")
        || ct.starts_with("font/")
    {
        return Err("unsupported_content_type");
    }
    Ok(())
}

pub fn extract_title_from_html(html: &str) -> Option<String> {
    let sample = if html.len() > 512_000 {
        &html[..512_000]
    } else {
        html
    };
    let doc = Html::parse_document(sample);
    let title_sel = Selector::parse("title").ok()?;
    let raw = doc.select(&title_sel).next()?.text().collect::<String>();
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn is_open_tag(head: &[u8], name: &[u8]) -> bool {
    if head.is_empty() || head[0] != b'<' {
        return false;
    }
    let mut j = 1usize;
    while j < head.len() && head[j].is_ascii_whitespace() {
        j += 1;
    }
    head[j..].len() >= name.len() && head[j..j + name.len()].eq_ignore_ascii_case(name)
}

fn find_subslice_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| {
        w.iter()
            .zip(needle.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
    })
}

/// Visible-text word count without full html2text (hot path for `/scrape` auto mode).
pub fn fast_visible_word_count(html: &str) -> usize {
    let b = html.as_bytes();
    let cap = b.len().min(600_000);
    let mut i = 0usize;
    let mut words = 0usize;
    let mut in_tag = false;
    while i < cap {
        let c = b[i];
        if in_tag {
            if c == b'>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }
        if c == b'<' {
            let head = &b[i..(i + 96).min(cap)];
            if is_open_tag(head, b"script") {
                let rest = &b[i..cap];
                if let Some(rel) = find_subslice_ci(rest, b"</script>") {
                    i += rel + 9;
                    continue;
                }
            }
            if is_open_tag(head, b"style") {
                if let Some(rel) = find_subslice_ci(&b[i..cap], b"</style>") {
                    i += rel + 8;
                    continue;
                }
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        while i < cap && !b[i].is_ascii_whitespace() && b[i] != b'<' {
            i += 1;
        }
        words += 1;
    }
    words
}

pub fn html_challenge_markers_hit(html: &str) -> bool {
    let cap = html.len().min(200_000);
    let slice = &html[..cap];
    let lower = slice.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
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
    MARKERS.iter().any(|m| lower.contains(m))
}

pub fn count_headings_and_code(html: &str) -> (usize, usize) {
    let sample = if html.len() > 400_000 {
        &html[..400_000]
    } else {
        html
    };
    let doc = Html::parse_document(sample);
    let h_ok = Selector::parse("h1, h2, h3, h4, h5, h6").ok();
    let pre_ok = Selector::parse("pre, code").ok();
    let h = h_ok
        .as_ref()
        .map(|s| doc.select(s).count())
        .unwrap_or(0);
    let c = pre_ok
        .as_ref()
        .map(|s| doc.select(s).count())
        .unwrap_or(0);
    (h, c)
}

pub fn unique_word_ratio(text: &str) -> f64 {
    let words: Vec<_> = text
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return 0.0;
    }
    let mut uniq = std::collections::HashSet::new();
    for w in &words {
        uniq.insert(w);
    }
    uniq.len() as f64 / words.len() as f64
}

pub fn boilerplate_ratio_from_html(html: &str, visible_words: usize) -> f64 {
    let len = html.len().max(1);
    let tag_heavy = (html.matches('<').count() as f64 / len as f64).min(1.0);
    let word_sparse = if visible_words < 80 {
        0.35
    } else {
        0.0
    };
    (tag_heavy * 0.7 + word_sparse).min(1.0)
}

pub fn html_has_selector(html: &str, selector: &str) -> bool {
    let Ok(sel) = Selector::parse(selector) else {
        return false;
    };
    let sample = if html.len() > 800_000 {
        &html[..800_000]
    } else {
        html
    };
    Html::parse_document(sample).select(&sel).next().is_some()
}

pub fn html_contains_text_ci(html: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let sample = if html.len() > 900_000 {
        &html[..900_000]
    } else {
        html
    };
    let n = needle.to_ascii_lowercase();
    sample.to_ascii_lowercase().contains(&n)
}

pub fn build_signals(
    page: &Page,
    html: &str,
    link_count: usize,
    main_score: f64,
    fallback: FallbackReason,
    truncated: bool,
) -> ScrapeSignals {
    let visible_words = fast_visible_word_count(html);
    let body_text = html_to_plainish(html);
    let uwr = unique_word_ratio(&body_text);
    let bpr = boilerplate_ratio_from_html(html, visible_words);
    let (hc, cc) = count_headings_and_code(html);
    let html_len = html.len().max(1);
    let content_density = (body_text.len() as f64 / html_len as f64).min(1.0);
    let link_density = if visible_words > 0 {
        (link_count as f64 / visible_words as f64).min(20.0) / 20.0
    } else {
        1.0
    };
    let blocked = if page.waf_check || html_challenge_markers_hit(html) {
        0.85
    } else if matches!(
        page.status_code.as_u16(),
        403 | 429 | 503 | 407 | 408 | 409 | 425 | 451
    ) {
        0.75
    } else if page.status_code.as_u16() >= 500 {
        0.55
    } else {
        0.0
    };
    ScrapeSignals {
        word_count: visible_words,
        char_count: body_text.chars().count(),
        unique_word_ratio: uwr,
        boilerplate_ratio: bpr,
        link_density,
        heading_count: hc,
        code_block_count: cc,
        link_count,
        content_density,
        main_content_score: main_score,
        blocked_likelihood: blocked,
        fallback_reason: fallback,
        truncated,
        language: cheap_language_hint(&body_text),
    }
}

fn cheap_language_hint(text: &str) -> Option<String> {
    let sample: String = text.chars().take(4000).collect();
    if sample.is_empty() {
        return None;
    }
    let non_latin = sample
        .chars()
        .filter(|c| !c.is_ascii_alphanumeric() && !c.is_whitespace())
        .count();
    if non_latin * 4 > sample.chars().count() {
        Some("und".to_string())
    } else {
        Some("en".to_string())
    }
}

fn html_to_plainish(html: &str) -> String {
    let sample = if html.len() > 200_000 {
        &html[..200_000]
    } else {
        html
    };
    let mut out = String::with_capacity(sample.len().min(50_000));
    let mut in_tag = false;
    for ch in sample.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn scrape_error_for_network_message(msg: &str) -> ScrapeError {
    let lower = msg.to_ascii_lowercase();
    let (code, retryable) = if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("not known")
    {
        ("dns_error", false)
    } else if lower.contains("tls") || lower.contains("certificate") {
        ("tls_error", false)
    } else if lower.contains("timed out") || lower.contains("timeout") {
        ("timeout", true)
    } else if lower.contains("connection refused") || lower.contains("unreachable") {
        ("http_error", true)
    } else {
        ("http_error", true)
    };
    ScrapeError {
        code,
        message: msg.to_string(),
        retryable,
    }
}
