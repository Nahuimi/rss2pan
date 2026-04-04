mod acgnx;
mod dmhy;
mod mikanani;
mod nyaa;
mod rsshub;

use anyhow::{anyhow, Context};
use curl::easy::{Easy, List};
use regex::Regex;
use reqwest::Method;
use rss::{Channel, Item};
use std::io::BufReader;
use std::{fs::File, path::PathBuf, time::Duration};
use tokio::{task, time::sleep};

pub use acgnx::*;
pub use dmhy::*;
pub use mikanani::*;
pub use nyaa::*;
pub use rsshub::*;

use crate::{
    request::{Ajax, CompatRssRequestContext},
    rss_config::RssConfig,
    utils::canonicalize_magnet,
};

const RSS_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const RSS_FETCH_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

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
    source: reqwest::Error,
}

impl RssFetchError {
    pub(crate) fn new(
        url: impl Into<String>,
        attempts: usize,
        stage: RssFetchStage,
        retry_exhausted: bool,
        source: reqwest::Error,
    ) -> Self {
        Self {
            url: url.into(),
            attempts,
            stage,
            retry_exhausted,
            source,
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
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RssRequestMode {
    Normal,
    Fresh,
    Compat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RssFetchAttempt {
    Reqwest(RssRequestMode),
    Libcurl,
}

#[derive(Debug)]
enum FetchFeedError {
    Request {
        stage: RssFetchStage,
        source: reqwest::Error,
        detail: Option<String>,
    },
    Other(anyhow::Error),
    Parse(rss::Error),
}

fn request_mode_for_attempt(attempt: usize, max_attempts: usize) -> RssRequestMode {
    if attempt == 1 {
        RssRequestMode::Normal
    } else if max_attempts > 2 && attempt == max_attempts {
        RssRequestMode::Compat
    } else {
        RssRequestMode::Fresh
    }
}

fn request_mode_label(mode: RssRequestMode) -> &'static str {
    match mode {
        RssRequestMode::Normal => "default request path",
        RssRequestMode::Fresh => "fresh connection",
        RssRequestMode::Compat => "compatibility fallback",
    }
}

fn attempt_label(attempt: RssFetchAttempt) -> &'static str {
    match attempt {
        RssFetchAttempt::Reqwest(mode) => request_mode_label(mode),
        RssFetchAttempt::Libcurl => "libcurl preferred path",
    }
}

fn default_reqwest_attempts(max_attempts: usize) -> Vec<RssFetchAttempt> {
    (1..=max_attempts)
        .map(|attempt| RssFetchAttempt::Reqwest(request_mode_for_attempt(attempt, max_attempts)))
        .collect()
}

fn rss_fetch_attempt_plan(prefer_libcurl_first: bool, max_attempts: usize) -> Vec<RssFetchAttempt> {
    let mut attempts = Vec::with_capacity(max_attempts + usize::from(prefer_libcurl_first));
    if prefer_libcurl_first {
        attempts.push(RssFetchAttempt::Libcurl);
    }
    attempts.extend(default_reqwest_attempts(max_attempts));
    attempts
}

fn should_try_curl_fallback(mode: RssRequestMode, detail: Option<&str>) -> bool {
    mode == RssRequestMode::Compat
        && detail.is_some_and(|detail| detail.contains("Worker threw exception |"))
}

fn should_try_compat_libcurl_rescue(
    attempt: RssFetchAttempt,
    detail: Option<&str>,
    already_tried_libcurl: bool,
) -> bool {
    matches!(attempt, RssFetchAttempt::Reqwest(mode) if should_try_curl_fallback(mode, detail))
        && !already_tried_libcurl
}

fn reqwest_attempt_number(attempts: &[RssFetchAttempt], current_index: usize) -> usize {
    attempts[..=current_index]
        .iter()
        .filter(|attempt| matches!(attempt, RssFetchAttempt::Reqwest(_)))
        .count()
}

fn libcurl_preference_for_url(ajax: &Ajax, url: &str) -> bool {
    match ajax.rss_prefers_libcurl_first(url) {
        Ok(value) => value,
        Err(err) => {
            log::warn!(
                "read rss transport preference failed for {}: {}; falling back to reqwest-first",
                url,
                err
            );
            false
        }
    }
}

fn note_non_rescue_outcome_if_needed(ajax: &Ajax, url: &str, prefer_libcurl_first: bool) {
    if !prefer_libcurl_first {
        if let Err(err) = ajax.note_rss_non_rescue_outcome(url) {
            log::warn!(
                "update rss transport preference failed for {}: {}",
                url,
                err
            );
        }
    }
}

fn note_reqwest_success(ajax: &Ajax, url: &str) {
    if let Err(err) = ajax.note_rss_reqwest_success(url) {
        log::warn!(
            "update rss transport preference failed for {}: {}",
            url,
            err
        );
    }
}

fn note_libcurl_rescue(ajax: &Ajax, url: &str) {
    if let Err(err) = ajax.note_rss_libcurl_rescue(url) {
        log::warn!(
            "update rss transport preference failed for {}: {}",
            url,
            err
        );
    }
}

fn retry_strategy_label(next_attempt: RssFetchAttempt) -> &'static str {
    match next_attempt {
        RssFetchAttempt::Reqwest(RssRequestMode::Normal) => "with the default request path",
        RssFetchAttempt::Reqwest(RssRequestMode::Fresh) => "with a fresh connection",
        RssFetchAttempt::Reqwest(RssRequestMode::Compat) => "with the compatibility fallback",
        RssFetchAttempt::Libcurl => "with the preferred libcurl path",
    }
}

fn retry_delay_for_reqwest_attempt(
    reqwest_attempt_number: usize,
    retry_delays: &[Duration],
) -> Duration {
    retry_delays
        .get(reqwest_attempt_number.saturating_sub(1))
        .copied()
        .unwrap_or(Duration::ZERO)
}

fn log_retry(
    url: &str,
    current_index: usize,
    total_attempts: usize,
    delay: Duration,
    next_attempt: RssFetchAttempt,
    err: &dyn std::fmt::Display,
) {
    let retry_strategy = retry_strategy_label(next_attempt);
    if delay.is_zero() {
        log::warn!(
            "retrying rss fetch attempt {}/{} for {} {} after {}",
            current_index + 2,
            total_attempts,
            url,
            retry_strategy,
            err
        );
    } else {
        log::warn!(
            "retrying rss fetch attempt {}/{} for {} in {:?} {} after {}",
            current_index + 2,
            total_attempts,
            url,
            delay,
            retry_strategy,
            err
        );
    }
}

async fn run_compat_libcurl_rescue(
    ajax: &Ajax,
    url: &str,
    timeout: Duration,
    prefer_libcurl_first: bool,
) -> anyhow::Result<Channel> {
    log::warn!("retrying rss fetch for {} with libcurl fallback", url);
    match fetch_feed_with_libcurl(ajax, url, timeout).await {
        Ok(channel) => {
            note_libcurl_rescue(ajax, url);
            Ok(channel)
        }
        Err(err) => {
            note_non_rescue_outcome_if_needed(ajax, url, prefer_libcurl_first);
            Err(err)
        }
    }
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
    let max_reqwest_attempts = retry_delays.len() + 1;
    let prefer_libcurl_first = libcurl_preference_for_url(ajax, url);
    let attempts = rss_fetch_attempt_plan(prefer_libcurl_first, max_reqwest_attempts);
    let total_attempts = attempts.len();
    let mut already_tried_libcurl = false;

    for (current_index, attempt) in attempts.iter().copied().enumerate() {
        match attempt {
            RssFetchAttempt::Libcurl => {
                already_tried_libcurl = true;
                match fetch_feed_with_libcurl(ajax, url, timeout).await {
                    Ok(channel) => return Ok(channel),
                    Err(err) => {
                        log::warn!(
                            "rss fetch {} failed for {}: {}",
                            attempt_label(attempt),
                            url,
                            err
                        );
                        if let Some(next_attempt) = attempts.get(current_index + 1).copied() {
                            log_retry(
                                url,
                                current_index,
                                total_attempts,
                                Duration::ZERO,
                                next_attempt,
                                &err,
                            );
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
            RssFetchAttempt::Reqwest(mode) => match fetch_feed_once(ajax, url, timeout, mode).await
            {
                Ok(channel) => {
                    note_reqwest_success(ajax, url);
                    return Ok(channel);
                }
                Err(FetchFeedError::Other(err)) => {
                    note_non_rescue_outcome_if_needed(ajax, url, prefer_libcurl_first);
                    return Err(err);
                }
                Err(FetchFeedError::Parse(err)) => {
                    note_non_rescue_outcome_if_needed(ajax, url, prefer_libcurl_first);
                    return Err(err.into());
                }
                Err(FetchFeedError::Request {
                    stage,
                    source,
                    detail,
                }) => {
                    if let Some(detail) = detail.as_deref() {
                        log::warn!(
                            "rss fetch {} got {} for {}: {}",
                            request_mode_label(mode),
                            stage.label(),
                            url,
                            detail
                        );
                    }

                    if should_try_compat_libcurl_rescue(
                        attempt,
                        detail.as_deref(),
                        already_tried_libcurl,
                    ) {
                        return run_compat_libcurl_rescue(ajax, url, timeout, prefer_libcurl_first)
                            .await;
                    }

                    let retryable = is_retryable_reqwest_error(&source);
                    if retryable {
                        if let Some(next_attempt) = attempts.get(current_index + 1).copied() {
                            let reqwest_attempt_number =
                                reqwest_attempt_number(&attempts, current_index);
                            let delay = retry_delay_for_reqwest_attempt(
                                reqwest_attempt_number,
                                retry_delays,
                            );
                            log_retry(
                                url,
                                current_index,
                                total_attempts,
                                delay,
                                next_attempt,
                                &source,
                            );
                            if !delay.is_zero() {
                                sleep(delay).await;
                            }
                            continue;
                        }
                        note_non_rescue_outcome_if_needed(ajax, url, prefer_libcurl_first);
                        return Err(RssFetchError::new(
                            url,
                            reqwest_attempt_number(&attempts, current_index),
                            stage,
                            true,
                            source,
                        )
                        .into());
                    }

                    note_non_rescue_outcome_if_needed(ajax, url, prefer_libcurl_first);
                    return Err(RssFetchError::new(
                        url,
                        reqwest_attempt_number(&attempts, current_index),
                        stage,
                        false,
                        source,
                    )
                    .into());
                }
            },
        }
    }

    unreachable!()
}

async fn fetch_feed_with_libcurl(
    ajax: &Ajax,
    url: &str,
    timeout: Duration,
) -> anyhow::Result<Channel> {
    let context = ajax.compat_rss_request_context(url)?;
    let url = url.to_string();
    let content =
        task::spawn_blocking(move || fetch_feed_bytes_with_libcurl(&url, timeout, context))
            .await
            .context("join libcurl rss fallback task failed")??;
    Channel::read_from(&content[..]).context("parse libcurl RSS response failed")
}

fn fetch_feed_bytes_with_libcurl(
    url: &str,
    timeout: Duration,
    context: CompatRssRequestContext,
) -> anyhow::Result<Vec<u8>> {
    let mut easy = Easy::new();
    easy.url(url)
        .with_context(|| format!("set libcurl url failed for rss fetch: {url}"))?;
    easy.get(true)
        .with_context(|| format!("set libcurl GET failed for rss fetch: {url}"))?;
    easy.follow_location(true)
        .context("enable libcurl redirect following failed")?;
    easy.useragent(context.user_agent)
        .context("set libcurl user agent failed")?;
    easy.connect_timeout(timeout)
        .context("set libcurl connect timeout failed")?;
    easy.timeout(timeout)
        .context("set libcurl timeout failed")?;
    easy.accept_encoding("identity")
        .context("set libcurl accept encoding failed")?;
    if let Some(proxy_url) = context.proxy_url.as_deref() {
        easy.proxy(proxy_url)
            .with_context(|| format!("set libcurl proxy failed: {proxy_url}"))?;
    }

    let mut header_list = List::new();
    for (name, value) in context.headers.iter() {
        let value = value
            .to_str()
            .with_context(|| format!("invalid header value for {}", name.as_str()))?;
        header_list
            .append(&format!("{}: {}", name.as_str(), value))
            .with_context(|| format!("append libcurl header failed: {}", name.as_str()))?;
    }
    easy.http_headers(header_list)
        .context("set libcurl headers failed")?;

    let mut content = Vec::new();
    {
        let mut transfer = easy.transfer();
        transfer
            .write_function(|data| {
                content.extend_from_slice(data);
                Ok(data.len())
            })
            .context("register libcurl write handler failed")?;
        transfer
            .perform()
            .with_context(|| format!("libcurl fallback request failed for {url}"))?;
    }

    let status = easy.response_code().context("read libcurl status failed")?;
    if !(200..300).contains(&status) {
        let detail = String::from_utf8_lossy(&content).trim().to_string();
        let detail = if detail.is_empty() {
            format!("http status {status}")
        } else {
            summarize_error_body(&detail)
        };
        return Err(anyhow!("libcurl fallback failed for {url}: {detail}"));
    }

    Ok(content)
}

#[cfg(test)]
async fn fetch_feed_with_libcurl_for_test(url: &str, timeout: Duration) -> anyhow::Result<Channel> {
    let context = CompatRssRequestContext {
        resolved: crate::request::ResolvedSiteConfig {
            host: url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(ToString::to_string))
                .unwrap_or_default(),
            parser: None,
            use_proxy: false,
        },
        headers: reqwest::header::HeaderMap::new(),
        proxy_url: None,
        user_agent: crate::request::USER_AGENT,
    };
    let content = task::spawn_blocking({
        let url = url.to_string();
        move || fetch_feed_bytes_with_libcurl(&url, timeout, context)
    })
    .await
    .context("join libcurl rss fallback task failed")??;
    Channel::read_from(&content[..]).context("parse libcurl RSS response failed")
}

async fn fetch_feed_once(
    ajax: &Ajax,
    url: &str,
    timeout: Duration,
    mode: RssRequestMode,
) -> Result<Channel, FetchFeedError> {
    let response = match mode {
        RssRequestMode::Normal => ajax.gen_req(Method::GET, url),
        RssRequestMode::Fresh => ajax.gen_fresh_req(Method::GET, url),
        RssRequestMode::Compat => ajax.gen_compat_rss_req(Method::GET, url),
    }
    .map_err(FetchFeedError::Other)?;
    let response =
        response
            .timeout(timeout)
            .send()
            .await
            .map_err(|source| FetchFeedError::Request {
                stage: RssFetchStage::Request,
                source,
                detail: None,
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
            source,
            detail,
        });
    }
    let content = response
        .bytes()
        .await
        .map_err(|source| FetchFeedError::Request {
            stage: RssFetchStage::Body,
            source,
            detail: None,
        })?;
    Channel::read_from(&content[..]).map_err(FetchFeedError::Parse)
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

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout()
        || err.is_connect()
        || err.is_body()
        || matches!(err.status(), Some(status) if status.is_server_error())
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
        Some(pattern) if pattern.starts_with('/') && pattern.ends_with('/') => Some(
            Regex::new(&pattern[1..pattern.len() - 1])
                .with_context(|| format!("invalid regex filter: {}", pattern))?,
        ),
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
    fn test_summarize_error_body_prefers_html_title_and_heading() {
        let body = r#"<!DOCTYPE html>
<html>
  <head>
    <title>500 Internal Server Error</title>
  </head>
  <body>
    <h1>Access denied</h1>
    <p>Please enable cookies.</p>
  </body>
</html>"#;
        let summary = summarize_error_body(body);
        assert_eq!(summary, "title=500 Internal Server Error; h1=Access denied");
    }

    #[test]
    fn test_summarize_error_body_decodes_html_entities() {
        let body = r#"<html><head><title>Just a moment &amp; retry</title></head></html>"#;
        let summary = summarize_error_body(body);
        assert_eq!(summary, "title=Just a moment & retry");
    }

    #[test]
    fn test_should_try_curl_fallback_only_for_worker_exception_on_compat_mode() {
        assert!(should_try_curl_fallback(
            RssRequestMode::Compat,
            Some("title=Worker threw exception | mikanani.kas.pub | Cloudflare")
        ));
        assert!(!should_try_curl_fallback(
            RssRequestMode::Fresh,
            Some("title=Worker threw exception | mikanani.kas.pub | Cloudflare")
        ));
        assert!(!should_try_curl_fallback(
            RssRequestMode::Compat,
            Some("title=Attention Required! | Cloudflare")
        ));
    }

    #[test]
    fn test_rss_fetch_attempt_plan_uses_reqwest_first_by_default() {
        let attempts = rss_fetch_attempt_plan(false, 3);
        assert_eq!(
            attempts,
            vec![
                RssFetchAttempt::Reqwest(RssRequestMode::Normal),
                RssFetchAttempt::Reqwest(RssRequestMode::Fresh),
                RssFetchAttempt::Reqwest(RssRequestMode::Compat),
            ]
        );
    }

    #[test]
    fn test_rss_fetch_attempt_plan_uses_libcurl_first_after_promotion() {
        let attempts = rss_fetch_attempt_plan(true, 3);
        assert_eq!(
            attempts,
            vec![
                RssFetchAttempt::Libcurl,
                RssFetchAttempt::Reqwest(RssRequestMode::Normal),
                RssFetchAttempt::Reqwest(RssRequestMode::Fresh),
                RssFetchAttempt::Reqwest(RssRequestMode::Compat),
            ]
        );
    }

    #[test]
    fn test_should_try_compat_libcurl_rescue_skips_second_libcurl_attempt() {
        assert!(!should_try_compat_libcurl_rescue(
            RssFetchAttempt::Reqwest(RssRequestMode::Compat),
            Some("title=Worker threw exception | mikanani.kas.pub | Cloudflare"),
            true,
        ));
    }

    #[tokio::test]
    async fn test_fetch_feed_with_libcurl_reads_local_rss() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = test_url(&listener);
        let app = Router::new().route("/rss", get(|| async { (StatusCode::OK, TEST_RSS_BODY) }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = fetch_feed_with_libcurl_for_test(&url, Duration::from_secs(5)).await;

        server.abort();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().title(), "test");
    }

    #[tokio::test]
    async fn test_fetch_feed_with_libcurl_reports_http_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = test_url(&listener);
        let app = Router::new().route(
            "/rss",
            get(|| async { (StatusCode::BAD_GATEWAY, "temporary error") }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let err = fetch_feed_with_libcurl_for_test(&url, Duration::from_secs(5))
            .await
            .unwrap_err();

        server.abort();
        assert!(err.to_string().contains("libcurl fallback failed for"));
        assert!(err.to_string().contains("temporary error"));
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
