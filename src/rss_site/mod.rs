mod acgnx;
mod dmhy;
mod mikanani;
mod nyaa;
mod rsshub;

use anyhow::{anyhow, Context};
use regex::Regex;
use rss::{Channel, Item};
use std::io::BufReader;
use std::{fs::File, path::PathBuf, time::Duration};
use tokio::time::sleep;
use wreq::Method;

pub use acgnx::*;
pub use dmhy::*;
pub use mikanani::*;
pub use nyaa::*;
pub use rsshub::*;

use crate::{request::Ajax, rss_config::RssConfig, utils::canonicalize_magnet};

const RSS_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const RSS_FETCH_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

pub trait MagnetSite {
    fn get_magnet(&self, item: &Item) -> Option<String>;

    fn get_magnet_item(&self, item: &Item) -> Option<MagnetItem> {
        Some(MagnetItem {
            title: item.title().map_or_else(String::new, |s| s.to_string()),
            link: item.link().map_or_else(String::new, |s| s.to_string()),
            magnet: canonicalize_magnet(&self.get_magnet(item)?),
            description: item.description().map(|s| s.to_string()),
            content: item.content().map(|s| s.to_string()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct MagnetItem {
    pub title: String,
    pub link: String,
    pub magnet: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RssFetchStage {
    Request,
    Status,
    Body,
}

impl RssFetchStage {
    fn label(self) -> &'static str {
        match self {
            Self::Request => "request rss failed",
            Self::Status => "rss returned non-success status",
            Self::Body => "read rss body failed",
        }
    }
}

#[derive(Debug)]
pub(crate) struct RssFetchError {
    url: String,
    attempts: usize,
    stage: RssFetchStage,
    retry_exhausted: bool,
    source: anyhow::Error,
}

impl RssFetchError {
    pub(crate) fn new(
        url: impl Into<String>,
        attempts: usize,
        stage: RssFetchStage,
        retry_exhausted: bool,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self {
            url: url.into(),
            attempts,
            stage,
            retry_exhausted,
            source: source.into(),
        }
    }

    fn retry_exhausted(&self) -> bool {
        self.retry_exhausted
    }
}

impl std::fmt::Display for RssFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.retry_exhausted {
            write!(
                f,
                "{} after {} attempts: {}",
                self.stage.label(),
                self.attempts,
                self.url
            )
        } else {
            write!(f, "{}: {}", self.stage.label(), self.url)
        }
    }
}

impl std::error::Error for RssFetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.root_cause())
    }
}

#[derive(Debug)]
enum FetchFeedError {
    Request {
        stage: RssFetchStage,
        retryable: bool,
        detail: Option<String>,
        source: anyhow::Error,
    },
    Parse {
        retryable: bool,
        detail: Option<String>,
        source: anyhow::Error,
    },
    Other(anyhow::Error),
}

pub fn is_retry_exhausted_rss_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RssFetchError>()
        .is_some_and(RssFetchError::retry_exhausted)
}

pub fn get_site(template: &str) -> Option<Box<dyn MagnetSite>> {
    match template {
        "mikanani" => Some(Box::new(Mikanani)),
        "nyaa" => Some(Box::new(Nyaa)),
        "dmhy" => Some(Box::new(Dmhy)),
        "acgnx" => Some(Box::new(Acgnx)),
        "rsshub" => Some(Box::new(Rsshub)),
        _ => None,
    }
}

pub async fn get_feed(ajax: &Ajax, url: &str) -> anyhow::Result<Channel> {
    get_feed_with_retry(ajax, url, RSS_FETCH_TIMEOUT, &RSS_FETCH_RETRY_DELAYS).await
}

async fn get_feed_with_retry(
    ajax: &Ajax,
    url: &str,
    timeout: Duration,
    retry_delays: &[Duration],
) -> anyhow::Result<Channel> {
    let max_attempts = retry_delays.len() + 1;

    for attempt in 1..=max_attempts {
        match fetch_feed_once(ajax, url, timeout).await {
            Ok(channel) => return Ok(channel),
            Err(FetchFeedError::Other(err)) => return Err(err),
            Err(FetchFeedError::Parse {
                retryable,
                detail,
                source,
            }) => {
                if retryable && attempt < max_attempts {
                    let delay = retry_delays[attempt - 1];
                    if let Some(detail) = detail.as_deref() {
                        log::warn!(
                            "rss fetch got invalid content for {} on attempt {}/{}: {}",
                            url,
                            attempt,
                            max_attempts,
                            detail
                        );
                    }
                    log_retry(url, attempt + 1, max_attempts, delay, &source);
                    if !delay.is_zero() {
                        sleep(delay).await;
                    }
                    continue;
                }
                return Err(RssFetchError::new(
                    url,
                    attempt,
                    RssFetchStage::Body,
                    retryable,
                    source,
                )
                .into());
            }
            Err(FetchFeedError::Request {
                stage,
                retryable,
                detail,
                source,
            }) => {
                if let Some(detail) = detail.as_deref() {
                    log::warn!(
                        "rss fetch got {} for {} on attempt {}/{}: {}",
                        stage.label(),
                        url,
                        attempt,
                        max_attempts,
                        detail
                    );
                }
                if retryable && attempt < max_attempts {
                    let delay = retry_delays[attempt - 1];
                    log_retry(url, attempt + 1, max_attempts, delay, &source);
                    if delay.is_zero() {
                        continue;
                    }
                    sleep(delay).await;
                    continue;
                }
                return Err(RssFetchError::new(url, attempt, stage, retryable, source).into());
            }
        }
    }

    unreachable!()
}

async fn fetch_feed_once(
    ajax: &Ajax,
    url: &str,
    timeout: Duration,
) -> Result<Channel, FetchFeedError> {
    let response = ajax
        .gen_rss_req(Method::GET, url)
        .map_err(FetchFeedError::Other)?;
    let response =
        response
            .timeout(timeout)
            .send()
            .await
            .map_err(|source| FetchFeedError::Request {
                stage: RssFetchStage::Request,
                retryable: is_retryable_wreq_error(&source),
                detail: None,
                source: anyhow::Error::new(source),
            })?;
    let status = response.status();
    if !status.is_success() {
        let source = response.error_for_status_ref().unwrap_err();
        let detail = response
            .text()
            .await
            .ok()
            .map(|body| summarize_error_body(&body));
        return Err(FetchFeedError::Request {
            stage: RssFetchStage::Status,
            retryable: is_retryable_status(status) || is_cloudflare_1101(detail.as_deref()),
            detail,
            source: anyhow::Error::new(source),
        });
    }
    let content = response
        .bytes()
        .await
        .map_err(|source| FetchFeedError::Request {
            stage: RssFetchStage::Body,
            retryable: is_retryable_wreq_error(&source),
            detail: None,
            source: anyhow::Error::new(source),
        })?;
    Channel::read_from(&content[..]).map_err(|source| {
        let body = String::from_utf8_lossy(&content);
        let detail = summarize_error_body(&body);
        FetchFeedError::Parse {
            retryable: is_cloudflare_1101(Some(&detail)),
            detail: Some(detail),
            source: anyhow::Error::new(source),
        }
    })
}

fn log_retry(
    url: &str,
    next_attempt: usize,
    max_attempts: usize,
    delay: Duration,
    err: &dyn std::fmt::Display,
) {
    if delay.is_zero() {
        log::warn!(
            "retrying rss fetch attempt {}/{} for {} after {}",
            next_attempt,
            max_attempts,
            url,
            err
        );
    } else {
        log::warn!(
            "retrying rss fetch attempt {}/{} for {} in {:?} after {}",
            next_attempt,
            max_attempts,
            url,
            delay,
            err
        );
    }
}

fn is_retryable_status(status: wreq::StatusCode) -> bool {
    status == wreq::StatusCode::REQUEST_TIMEOUT
        || status == wreq::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn is_retryable_wreq_error(err: &wreq::Error) -> bool {
    err.is_timeout()
        || err.is_connect()
        || err.is_body()
        || matches!(err.status(), Some(status) if status.is_server_error())
}

fn is_cloudflare_1101(detail: Option<&str>) -> bool {
    detail.is_some_and(|detail| {
        let lower = detail.to_ascii_lowercase();
        lower.contains("1101")
            || lower.contains("worker threw exception")
            || (lower.contains("cloudflare") && lower.contains("worker"))
    })
}

fn summarize_error_body(body: &str) -> String {
    if looks_like_html(body) {
        let mut parts = Vec::new();
        if let Some(title) = extract_html_tag_text(body, "title") {
            parts.push(format!("title={title}"));
        }
        if let Some(h1) = extract_html_tag_text(body, "h1") {
            let heading = format!("h1={h1}");
            if !parts.contains(&heading) {
                parts.push(heading);
            }
        }
        if !parts.is_empty() {
            return truncate_summary(&parts.join("; "));
        }
    }

    truncate_summary(&compact_whitespace(body))
}

fn looks_like_html(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("<!doctype html") || lower.contains("<html")
}

fn extract_html_tag_text(body: &str, tag: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let open_tag = format!("<{tag}");
    let start = lower.find(&open_tag)?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close_tag = format!("</{tag}>");
    let end = lower[open_end..].find(&close_tag)? + open_end;
    let text = compact_whitespace(&decode_common_html_entities(&strip_html_tags(
        &body[open_end..end],
    )));
    (!text.is_empty()).then_some(text)
}

fn strip_html_tags(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => {
                in_tag = true;
                output.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn decode_common_html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_summary(text: &str) -> String {
    let mut summary = text.chars().take(240).collect::<String>();
    if text.chars().count() > 240 {
        summary.push_str("...");
    }
    summary
}

pub fn get_magnet_by_enclosure(item: &Item) -> String {
    item.enclosure()
        .map(|enclosure| {
            enclosure
                .url
                .split("&dn=")
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default()
}

pub async fn get_magnetitem_list(
    ajax: &Ajax,
    config: &RssConfig,
) -> anyhow::Result<Vec<MagnetItem>> {
    let resolved = ajax.resolve_site_by_url(&config.url)?;
    let parser = resolved
        .parser
        .ok_or_else(|| anyhow!("not support site: {}", config.url))?;
    let Some(site) = get_site(&parser) else {
        return Err(anyhow!("not support site parser: {parser}"));
    };

    let channel = get_feed(ajax, &config.url).await?;
    let regex = match config.filter.as_deref() {
        Some(pattern)
            if pattern.len() >= 2 && pattern.starts_with('/') && pattern.ends_with('/') =>
        {
            Some(
                Regex::new(&pattern[1..pattern.len() - 1])
                    .with_context(|| format!("invalid regex filter: {}", pattern))?,
            )
        }
        _ => None,
    };

    let mut invalid_items = 0usize;
    let items = channel
        .items()
        .iter()
        .filter_map(|item| match site.get_magnet_item(item) {
            Some(item) => Some(item),
            None => {
                invalid_items += 1;
                log::warn!(
                    "skip invalid rss item from [{}]: title={}",
                    config.url,
                    item.title().unwrap_or_default()
                );
                None
            }
        })
        .filter(|item| match (config.filter.as_deref(), regex.as_ref()) {
            (Some(_), Some(regex)) => regex.is_match(&item.title),
            (Some(pattern), None) => item.title.contains(pattern),
            (None, _) => true,
        })
        .collect::<Vec<_>>();

    if invalid_items > 0 {
        log::warn!(
            "[{}] skipped {} invalid rss items",
            config.url,
            invalid_items
        );
    }

    Ok(items)
}

#[allow(dead_code)]
pub fn get_feed_by_file(path: PathBuf) -> anyhow::Result<Channel> {
    let file = File::open(path).expect("no such file");
    let buf_reader = BufReader::new(file);
    let channel = Channel::read_from(buf_reader)?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RssService;
    use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::net::TcpListener;

    const TEST_RSS_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>test</title>
    <link>https://example.com</link>
    <description>test feed</description>
    <item>
      <title>item</title>
      <link>https://example.com/item</link>
      <description>item</description>
    </item>
  </channel>
</rss>"#;

    fn test_url(listener: &TcpListener) -> String {
        format!("http://{}/rss", listener.local_addr().unwrap())
    }

    #[tokio::test]
    async fn test_get_feed_retries_server_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = test_url(&listener);
        let app = Router::new().route(
            "/rss",
            get({
                let attempts = attempts.clone();
                move || async move {
                    let current = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if current < 3 {
                        (StatusCode::BAD_GATEWAY, "temporary error").into_response()
                    } else {
                        (StatusCode::OK, TEST_RSS_BODY).into_response()
                    }
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let ajax = Ajax::new(None).unwrap();
        let result = get_feed_with_retry(
            &ajax,
            &url,
            Duration::from_millis(200),
            &[Duration::ZERO, Duration::ZERO],
        )
        .await;

        server.abort();
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_get_feed_marks_timeout_as_retry_exhausted() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = test_url(&listener);
        let app = Router::new().route(
            "/rss",
            get({
                let attempts = attempts.clone();
                move || async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    sleep(Duration::from_millis(50)).await;
                    (StatusCode::OK, TEST_RSS_BODY)
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let ajax = Ajax::new(None).unwrap();
        let err = get_feed_with_retry(
            &ajax,
            &url,
            Duration::from_millis(10),
            &[Duration::ZERO, Duration::ZERO],
        )
        .await
        .unwrap_err();

        server.abort();
        assert!(is_retry_exhausted_rss_error(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_get_feed_does_not_retry_client_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = test_url(&listener);
        let app = Router::new().route(
            "/rss",
            get({
                let attempts = attempts.clone();
                move || async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::NOT_FOUND, "missing")
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let ajax = Ajax::new(None).unwrap();
        let err = get_feed_with_retry(
            &ajax,
            &url,
            Duration::from_millis(200),
            &[Duration::ZERO, Duration::ZERO],
        )
        .await
        .unwrap_err();

        server.abort();
        assert!(!is_retry_exhausted_rss_error(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_db_save_items() {
        let channel = get_feed_by_file("tests/Bangumi.rss".into());
        assert!(channel.is_ok());
        let channel = channel.unwrap();

        let mut service = RssService::new_in_memory().unwrap();
        let site = get_site("mikanani").unwrap();
        let items: Vec<MagnetItem> = channel
            .items()
            .iter()
            .filter_map(|item| site.get_magnet_item(item))
            .collect();
        let res = service.save_items(&items, true);
        assert!(res.is_ok());
    }

    #[test]
    fn test_get_acgnx_items() {
        let channel = get_feed_by_file("tests/acgnx.rss".into());
        assert!(channel.is_ok());
        let channel = channel.unwrap();
        let site = get_site("acgnx").unwrap();
        let items: Vec<MagnetItem> = channel
            .items()
            .iter()
            .filter_map(|item| site.get_magnet_item(item))
            .collect();
        assert_eq!(items.len(), 50);
        assert_eq!(
            items[0].magnet,
            "magnet:?xt=urn:btih:4355c456f7b03ea007e998d101f858087daf4d26"
        );
    }

    #[test]
    fn test_re() {
        let str_list = [
            "[7月新番][传颂之物 二人的白皇][Utawarerumono - Futari no Hakuoro][09][1080P][MP4][GB][简中] [241.72 MB]",
            "【幻樱字幕组】【7月新番】【传颂之物 二人白皇 Utawarerumono-Futari no Hakuoro-】【16】【BIG5_MP4】【1920X1080】 [321.13 MB]",
            "[动漫国字幕组&澄空学园&LoliHouse] 传颂之物 二人的白皇 / Utawarerumono Futari no Hakuoro - 16 [WebRip 1080p HEVC-10bit AAC][简繁外挂字幕] [485.4 MB]"
        ];
        let pat = "/澄空学园|幻樱|\\d{4}[pP]/";
        let re = Regex::new(&pat[1..pat.len() - 1]).unwrap();
        assert_eq!(str_list.map(|s| re.is_match(s)), [true, true, true]);
    }
}
