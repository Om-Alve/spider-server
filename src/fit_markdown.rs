//! `raw_markdown` + `fit_markdown` for LLM pipelines, modeled on Crawl4AI’s `PruningContentFilter`.

use std::sync::OnceLock;

use htmd::HtmlToMarkdown;
use lol_html::{element, rewrite_str, RewriteStrSettings};
use scraper::{ElementRef, Html, Selector};

static MD: OnceLock<HtmlToMarkdown> = OnceLock::new();

fn md() -> &'static HtmlToMarkdown {
    MD.get_or_init(|| {
        HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "svg", "noscript"])
            .build()
    })
}

const NEG_PATTERNS: [&str; 10] = [
    "nav", "footer", "header", "sidebar", "ads", "comment", "promo", "advert", "social", "share",
];

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr"
    )
}

fn is_negative_class_or_id(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    NEG_PATTERNS.iter().any(|m| s.contains(m))
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruningType {
    #[default]
    Fixed,
    Dynamic,
}

/// Optional tuning (defaults are Crawl4AI PruningContentFilter–like: threshold ~0.48, min words 5).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FitMarkdownOptions {
    pub pruning_threshold: f64,
    pub pruning_type: PruningType,
    pub min_word_threshold: Option<usize>,
}

impl Default for FitMarkdownOptions {
    fn default() -> Self {
        Self {
            pruning_threshold: 0.48,
            pruning_type: PruningType::Fixed,
            min_word_threshold: Some(5),
        }
    }
}

struct PruningConfig {
    threshold: f64,
    threshold_type: bool, // true = fixed
    min_word_threshold: Option<usize>,
}

const TAG_I: &[(&str, f64)] = &[
    ("article", 1.5),
    ("main", 1.4),
    ("section", 1.3),
    ("p", 1.2),
    ("h1", 1.4),
    ("h2", 1.3),
    ("h3", 1.2),
    ("div", 0.7),
    ("span", 0.6),
];
const TAG_W: &[(&str, f64)] = &[
    ("div", 0.5),
    ("p", 1.0),
    ("article", 1.5),
    ("section", 1.0),
    ("span", 0.3),
    ("li", 0.5),
    ("ul", 0.5),
    ("ol", 0.5),
    ("h1", 1.2),
    ("h2", 1.1),
    ("h3", 1.0),
    ("h4", 0.9),
    ("h5", 0.8),
    ("h6", 0.7),
];

fn lk(needle: &str, t: &[(&str, f64)]) -> f64 {
    t.iter()
        .find_map(|(n, w)| (*n == needle).then_some(*w))
        .unwrap_or(0.5)
}

impl PruningConfig {
    fn from_opts(o: &FitMarkdownOptions) -> Self {
        Self {
            threshold: o.pruning_threshold,
            threshold_type: matches!(o.pruning_type, PruningType::Fixed),
            min_word_threshold: o.min_word_threshold,
        }
    }

    fn class_pos(&self, el: &ElementRef<'_>) -> f64 {
        let v = el.value();
        let mut s = 0.0f64;
        if v.attr("class").is_some_and(is_negative_class_or_id) {
            s -= 0.5;
        }
        if v.attr("id").is_some_and(is_negative_class_or_id) {
            s -= 0.5;
        }
        s.max(0.0)
    }

    fn direct_a_len(el: ElementRef<'_>) -> usize {
        el.child_elements()
            .filter(|c| c.value().name() == "a")
            .map(|a| a.text().map(str::len).sum::<usize>())
            .sum()
    }

    fn score(&self, el: &ElementRef<'_>, name: &str) -> f64 {
        let t = el.text().collect::<Vec<_>>().join(" ");
        if let Some(mw) = self.min_word_threshold {
            if t.split_whitespace().count() < mw {
                return -1.0;
            }
        }
        let tlen = t.len().max(1);
        let hlen = el.inner_html().len().max(1);
        let la = Self::direct_a_len(*el) as f64;
        let wd = 0.4f64;
        let wk = 0.2f64;
        let wtag = 0.2f64;
        let wcls = 0.1f64;
        let wln = 0.1f64;
        let mut s = 0.0f64;
        let mut wsum = 0.0f64;
        s += wd * (tlen as f64 / hlen as f64);
        wsum += wd;
        s += wk * (1.0 - (la / tlen as f64).min(1.0));
        wsum += wk;
        s += wtag * lk(name, TAG_W);
        wsum += wtag;
        s += wcls * self.class_pos(el);
        wsum += wcls;
        s += wln * f64::ln((t.len() + 1) as f64);
        wsum += wln;
        s / wsum
    }

    fn eff_t(&self, n: &str, tl: usize, hl: usize, ll: usize) -> f64 {
        if self.threshold_type {
            return self.threshold;
        }
        let i = lk(n, TAG_I);
        let tr = if hl > 0 {
            tl as f64 / hl as f64
        } else {
            0.0
        };
        let lr = if tl > 0 {
            ll as f64 / tl as f64
        } else {
            1.0
        };
        let mut t = self.threshold;
        if i > 1.0 {
            t *= 0.8;
        }
        if tr > 0.4 {
            t *= 0.9;
        }
        if lr > 0.6 {
            t *= 1.2;
        }
        t
    }

    fn remove(&self, el: &ElementRef<'_>, name: &str, t: &str, ll: usize, sc: f64) -> bool {
        if sc < 0.0 {
            return true;
        }
        let eff = self.eff_t(
            name,
            t.len().max(1),
            el.inner_html().len().max(1),
            ll,
        );
        sc < eff
    }
}

/// JSON shape mirroring Crawl4AI: `raw_markdown`, `fit_markdown`, `fit_html`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FitMarkdown {
    pub raw_markdown: String,
    pub fit_markdown: String,
    pub fit_html: String,
}

fn strip_excluded_hostile(html: &str) -> String {
    const SEL: &str = "script, style, link[rel=stylesheet], nav, footer, header, aside, form, \
                        iframe, noscript, [role=navigation], [role=contentinfo], [data-ad], \
                        [class*=ad-], [id*=ad-], [class*=ads]";
    rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![element!(SEL, |e| {
                e.remove();
                Ok(())
            })],
            ..RewriteStrSettings::new()
        },
    )
    .unwrap_or_else(|_| html.to_string())
}

fn esc_text(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            _ => c.to_string(),
        })
        .collect()
}

fn sq(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '"' => "&quot;".into(),
            c => c.to_string(),
        })
        .collect()
}

fn void_tag_open(el: &ElementRef<'_>) -> String {
    match el.value().name() {
        "br" => "<br>".into(),
        "img" => {
            if let Some(s) = el.value().attr("src") {
                if let Some(a) = el.value().attr("alt") {
                    format!("<img src=\"{}\" alt=\"{}\">", sq(s), sq(a))
                } else {
                    format!("<img src=\"{}\">", sq(s))
                }
            } else {
                "<img>".into()
            }
        }
        "hr" => "<hr>".into(),
        "input" | "meta" | "link" | "wbr" | "area" | "base" | "col" | "embed" | "param" | "source" | "track" => {
            let n = el.value().name();
            format!("<{n}>")
        }
        _ => format!("<{}>", el.value().name()),
    }
}

fn ser(el: ElementRef<'_>, c: &PruningConfig) -> String {
    let n = el.value().name();
    let text = el.text().collect::<Vec<_>>().join(" ");
    let lla = PruningConfig::direct_a_len(el);
    let sc = c.score(&el, n);
    if c.remove(&el, n, &text, lla, sc) {
        return String::new();
    }
    if is_void(n) {
        return void_tag_open(&el);
    }
    let o = if n == "a" {
        if let Some(h) = el.value().attr("href") {
            format!("<a href=\"{}\">", sq(h))
        } else {
            "<a>".into()
        }
    } else {
        format!("<{n}>")
    };
    let mut inr = String::new();
    for ch in el.children() {
        if let Some(t) = ch.value().as_text() {
            inr.push_str(&esc_text(t));
        } else if let Some(s) = ElementRef::wrap(ch) {
            inr.push_str(&ser(s, c));
        }
    }
    o + &inr + &format!("</{n}>")
}

/// Same selectors as the server’s `extract_primary_html_block` (main / article / content).
fn best_semantic_fragment(stripped: &str) -> Option<String> {
    let doc = Html::parse_document(stripped);
    let sels = [
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
    for s in sels {
        let Ok(sel) = Selector::parse(s) else {
            continue;
        };
        for el in doc.select(&sel) {
            let w = el.text().collect::<Vec<_>>().join(" ").split_whitespace().count();
            if w < 40 {
                continue;
            }
            let f = el.html();
            let update = match &best {
                None => true,
                Some((best_w, _)) => w > *best_w,
            };
            if update {
                best = Some((w, f));
            }
        }
    }
    best.map(|(_, h)| h)
}

fn body_fragment(stripped: &str) -> String {
    let d = Html::parse_document(stripped);
    let bsel = match Selector::parse("body") {
        Ok(s) => s,
        Err(_) => return stripped.to_string(),
    };
    d.select(&bsel)
        .next()
        .map(|b| b.html())
        .unwrap_or_else(|| stripped.to_string())
}

pub fn build_fit_markdown(
    full_html: &str,
    opts: &FitMarkdownOptions,
) -> std::io::Result<FitMarkdown> {
    let c = PruningConfig::from_opts(opts);
    let stripped = strip_excluded_hostile(full_html);
    let raw_markdown = md().convert(&stripped)?;
    let target = if let Some(m) = best_semantic_fragment(&stripped) {
        m
    } else {
        body_fragment(&stripped)
    };
    let frag = Html::parse_fragment(&target);
    let fit_root = frag.root_element();
    let mut fit_html = ser(fit_root, &c);
    if fit_html.split_whitespace().count() < 10 {
        fit_html = target;
    }
    let fit_markdown = md().convert(&fit_html)?;
    Ok(FitMarkdown {
        raw_markdown,
        fit_markdown,
        fit_html,
    })
}
