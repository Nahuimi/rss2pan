use std::{collections::HashSet, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::ArgMatches;
use futures::{stream, StreamExt};
use regex::Regex;
use tokio::time::sleep;

use crate::{
    db::{BlacklistService, RssService},
    pan115::{Pan115Client, Pan115Error, Pan115ErrorKind},
    request::Ajax,
    rss_config::{get_rss_config_by_url, get_rss_list, RssConfig},
    rss_site::{get_magnetitem_list, is_retry_exhausted_rss_error, MagnetItem},
    utils::canonicalize_magnet,
};

const RSS_FETCH_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub disable_cache: bool,
    pub chunk_size: usize,
    pub chunk_delay: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            disable_cache: false,
            chunk_size: 200,
            chunk_delay: Duration::from_secs(2),
        }
    }
}

impl RunOptions {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        let mut options = Self {
            disable_cache: matches
                .get_one::<bool>("no-cache")
                .copied()
                .unwrap_or(false),
            ..Self::default()
        };
        if let Some(chunk_size) = matches.get_one::<usize>("chunk-size").copied() {
            options.chunk_size = chunk_size.max(1);
        }
        if let Some(chunk_delay) = matches.get_one::<u64>("chunk-delay").copied() {
            options.chunk_delay = Duration::from_secs(chunk_delay);
        }
        options
    }
}

pub struct TaskRunner {
    pan115: Pan115Client,
    ajax: Ajax,
    rss_path: Option<PathBuf>,
    options: RunOptions,
}

impl TaskRunner {
    pub fn new(
        pan115: Pan115Client,
        ajax: Ajax,
        rss_path: Option<PathBuf>,
        options: RunOptions,
    ) -> Self {
        Self {
            pan115,
            ajax,
            rss_path,
            options,
        }
    }

    pub fn options(&self) -> RunOptions {
        self.options
    }

    pub async fn execute_url(
        &self,
        service: &mut RssService,
        blacklist: &BlacklistService,
        url: &str,
    ) -> Result<()> {
        let config = get_rss_config_by_url(self.rss_path.as_ref(), url)?;
        if should_skip_blacklisted_rss(blacklist, &config)? {
            return Ok(());
        }
        self.pan115.ensure_logged_in().await?;
        let item_list = get_magnetitem_list(&self.ajax, &config).await?;
        self.execute_task(service, blacklist, &item_list, &config)
            .await
    }

    pub async fn execute_all(
        &self,
        service: &mut RssService,
        blacklist: &BlacklistService,
    ) -> Result<()> {
        let configs = filter_blacklisted_configs(blacklist, get_rss_list(self.rss_path.as_ref())?)?;
        if configs.is_empty() {
            return Ok(());
        }
        self.pan115.ensure_logged_in().await?;
        for config in &configs {
            match get_magnetitem_list(&self.ajax, config).await {
                Ok(item_list) => {
                    self.execute_task(service, blacklist, &item_list, config)
                        .await?
                }
                Err(err) if should_skip_rss_error(&err) => {
                    log::warn!(
                        "[{}] skip rss source after retries exhausted: {}",
                        config_name_or_url(config),
                        err
                    );
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    pub async fn execute_all_concurrent(
        &self,
        service: &mut RssService,
        blacklist: &BlacklistService,
    ) -> Result<()> {
        let configs = filter_blacklisted_configs(blacklist, get_rss_list(self.rss_path.as_ref())?)?;
        if configs.is_empty() {
            return Ok(());
        }
        self.pan115.ensure_logged_in().await?;

        let mut stream = stream::iter(configs.into_iter().map(|config| {
            let ajax = self.ajax.clone();
            async move {
                let result = get_magnetitem_list(&ajax, &config).await;
                (config, result)
            }
        }))
        .buffer_unordered(RSS_FETCH_CONCURRENCY);

        while let Some((config, result)) = stream.next().await {
            match result {
                Ok(item_list) => {
                    self.execute_task(service, blacklist, &item_list, &config)
                        .await?
                }
                Err(err) if is_retry_exhausted_rss_error(&err) => {
                    log::warn!(
                        "[{}] skip rss source after retries exhausted: {}",
                        config_name_or_url(&config),
                        err
                    );
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    pub async fn execute_links(
        &self,
        links: &[String],
        cid: Option<String>,
        savepath: Option<String>,
    ) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        submit_links_with_options(
            &self.pan115,
            self.options,
            "[magnet]",
            links,
            cid.as_deref(),
            savepath.as_deref(),
        )
        .await
    }

    async fn execute_task(
        &self,
        service: &mut RssService,
        blacklist: &BlacklistService,
        item_list: &[MagnetItem],
        config: &RssConfig,
    ) -> Result<()> {
        let (deduped, empty_num) = dedup_task_items(item_list);
        let app_config = self.ajax.app_config();
        let blacklist_candidate =
            find_blacklist_candidate(&deduped, app_config.blacklist.exclude_pattern())?;

        if empty_num > 0 {
            log::warn!(
                "[{}] has {} empty tasks",
                config_name_or_url(config),
                empty_num
            );
        }

        if deduped.is_empty() {
            log::info!("[{}] has 0 task", config_name_or_url(config));
            return Ok(());
        }

        let tasks = if self.options.disable_cache {
            deduped
        } else {
            let magnets = deduped
                .iter()
                .map(|item| item.magnet.clone())
                .collect::<Vec<_>>();
            let existing = service.existing_magnets(&magnets)?;
            deduped
                .into_iter()
                .filter(|item| !existing.contains(&item.magnet))
                .collect::<Vec<_>>()
        };

        if tasks.is_empty() {
            log::info!("[{}] has 0 task", config_name_or_url(config));
            blacklist_matched_rss(blacklist, config, blacklist_candidate.as_ref())?;
            return Ok(());
        }

        let total_chunks = chunk_count(tasks.len(), self.options.chunk_size);
        for (index, chunk) in tasks.chunks(self.options.chunk_size).enumerate() {
            let links = chunk
                .iter()
                .map(|item| item.magnet.clone())
                .collect::<Vec<_>>();
            match submit_links_chunk(
                &self.pan115,
                &links,
                config.cid.as_deref(),
                config.savepath.as_deref(),
            )
            .await
            {
                Ok(SubmitChunkOutcome::Added) => {
                    log::info!(
                        "[{}] [{}] add {} tasks",
                        config_name_or_url(config),
                        config.url,
                        chunk.len()
                    );
                    service.save_items(chunk, true)?;
                }
                Ok(SubmitChunkOutcome::TaskExisted) => {
                    log::warn!("[{}] task exist", config_name_or_url(config));
                    service.save_items(chunk, true)?;
                }
                Err(err) if matches!(classify_add_error(&err), AddErrorAction::FailInvalidLink) => {
                    log::warn!("[{}] wrong links", config_name_or_url(config));
                    return Err(err);
                }
                Err(err) => return Err(err),
            }
            if index + 1 < total_chunks {
                sleep(self.options.chunk_delay).await;
            }
        }

        blacklist_matched_rss(blacklist, config, blacklist_candidate.as_ref())?;
        Ok(())
    }
}

pub async fn submit_links_with_options(
    pan115: &Pan115Client,
    options: RunOptions,
    label: &str,
    links: &[String],
    cid: Option<&str>,
    savepath: Option<&str>,
) -> Result<()> {
    let total_chunks = chunk_count(links.len(), options.chunk_size);
    for (index, chunk) in links.chunks(options.chunk_size).enumerate() {
        match submit_links_chunk(pan115, chunk, cid, savepath).await {
            Ok(SubmitChunkOutcome::Added) => {
                log::info!("{label} add {} tasks", chunk.len());
            }
            Ok(SubmitChunkOutcome::TaskExisted) => {
                log::warn!("{label} task exist");
            }
            Err(err) if matches!(classify_add_error(&err), AddErrorAction::FailInvalidLink) => {
                log::warn!("{label} wrong links");
                return Err(err);
            }
            Err(err) => return Err(err),
        }
        if index + 1 < total_chunks {
            sleep(options.chunk_delay).await;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitChunkOutcome {
    Added,
    TaskExisted,
}

async fn submit_links_chunk(
    pan115: &Pan115Client,
    links: &[String],
    cid: Option<&str>,
    savepath: Option<&str>,
) -> Result<SubmitChunkOutcome> {
    match pan115.add_offline_urls(links, cid, savepath).await {
        Ok(_) => Ok(SubmitChunkOutcome::Added),
        Err(err) => match classify_add_error(&err) {
            AddErrorAction::IgnoreTaskExisted => Ok(SubmitChunkOutcome::TaskExisted),
            _ => Err(err),
        },
    }
}

fn dedup_task_items(item_list: &[MagnetItem]) -> (Vec<MagnetItem>, usize) {
    let mut empty_num = 0usize;
    let mut deduped = Vec::new();
    let mut seen_magnets = HashSet::new();

    for item in item_list {
        let magnet = canonicalize_magnet(&item.magnet);
        if magnet.is_empty() {
            empty_num += 1;
            continue;
        }
        if seen_magnets.insert(magnet.clone()) {
            let mut item = item.clone();
            item.magnet = magnet;
            deduped.push(item);
        }
    }

    (deduped, empty_num)
}

pub fn submit_error_kind(err: &anyhow::Error) -> Option<Pan115ErrorKind> {
    add_error_kind(err)
}

fn add_error_kind(err: &anyhow::Error) -> Option<Pan115ErrorKind> {
    err.downcast_ref::<Pan115Error>().map(Pan115Error::kind)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddErrorAction {
    IgnoreTaskExisted,
    FailInvalidLink,
    Fail,
}

fn classify_add_error(err: &anyhow::Error) -> AddErrorAction {
    match add_error_kind(err) {
        Some(Pan115ErrorKind::TaskExisted) => AddErrorAction::IgnoreTaskExisted,
        Some(Pan115ErrorKind::InvalidLink) => AddErrorAction::FailInvalidLink,
        _ => AddErrorAction::Fail,
    }
}

fn should_skip_rss_error(err: &anyhow::Error) -> bool {
    is_retry_exhausted_rss_error(err)
}

fn filter_blacklisted_configs(
    blacklist: &BlacklistService,
    configs: Vec<RssConfig>,
) -> Result<Vec<RssConfig>> {
    let mut filtered = Vec::with_capacity(configs.len());
    for config in configs {
        if should_skip_blacklisted_rss(blacklist, &config)? {
            continue;
        }
        filtered.push(config);
    }
    Ok(filtered)
}

fn should_skip_blacklisted_rss(blacklist: &BlacklistService, config: &RssConfig) -> Result<bool> {
    let is_blacklisted = blacklist.contains_rss_url(&config.url)?;
    if is_blacklisted {
        log::info!(
            "[{}] skip blacklisted rss source: {}",
            config_name_or_url(config),
            config.url
        );
    }
    Ok(is_blacklisted)
}

fn find_blacklist_candidate(
    items: &[MagnetItem],
    pattern: Option<&str>,
) -> Result<Option<MagnetItem>> {
    let Some(pattern) = pattern else {
        return Ok(None);
    };
    let regex = compile_rule_regex(pattern, "blacklist.exclude")?;
    Ok(items
        .iter()
        .find(|item| title_matches_rule(&item.title, pattern, regex.as_ref()))
        .cloned())
}

fn blacklist_matched_rss(
    blacklist: &BlacklistService,
    config: &RssConfig,
    item: Option<&MagnetItem>,
) -> Result<()> {
    let Some(item) = item else {
        return Ok(());
    };
    blacklist.blacklist_rss(&config.url, item)?;
    log::info!(
        "[{}] blacklist rss source after exclude match: {} ({})",
        config_name_or_url(config),
        config.url,
        item.title
    );
    Ok(())
}

fn compile_rule_regex(pattern: &str, field_name: &str) -> Result<Option<Regex>> {
    if pattern.len() >= 2 && pattern.starts_with('/') && pattern.ends_with('/') {
        return Ok(Some(
            Regex::new(&pattern[1..pattern.len() - 1])
                .with_context(|| format!("invalid {field_name} regex: {pattern}"))?,
        ));
    }
    Ok(None)
}

fn title_matches_rule(title: &str, pattern: &str, regex: Option<&Regex>) -> bool {
    regex.is_some_and(|regex| regex.is_match(title)) || regex.is_none() && title.contains(pattern)
}

pub fn chunk_count(len: usize, size: usize) -> usize {
    if len == 0 {
        0
    } else {
        (len - 1) / size + 1
    }
}

fn config_name_or_url(config: &RssConfig) -> &str {
    if config.name.is_empty() {
        &config.url
    } else {
        &config.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn magnet_item(magnet: &str) -> MagnetItem {
        MagnetItem {
            title: "title".to_string(),
            link: "link".to_string(),
            magnet: magnet.to_string(),
            description: None,
            content: None,
        }
    }

    fn named_config(url: &str) -> RssConfig {
        RssConfig {
            name: "test".to_string(),
            url: url.to_string(),
            ..RssConfig::default()
        }
    }

    #[test]
    fn test_classify_add_error_ignores_task_existed() {
        let err: anyhow::Error = Pan115Error::new(10008, None).into();
        assert_eq!(classify_add_error(&err), AddErrorAction::IgnoreTaskExisted);
    }

    #[test]
    fn test_classify_add_error_rejects_invalid_link() {
        let err: anyhow::Error = Pan115Error::new(10004, None).into();
        assert_eq!(classify_add_error(&err), AddErrorAction::FailInvalidLink);
    }

    #[test]
    fn test_chunk_count_rounds_up() {
        assert_eq!(chunk_count(0, 200), 0);
        assert_eq!(chunk_count(1, 200), 1);
        assert_eq!(chunk_count(201, 200), 2);
    }

    #[test]
    fn test_dedup_task_items_normalizes_and_deduplicates() {
        let (items, empty_num) = dedup_task_items(&[
            magnet_item("magnet:?xt=URN:BTIH:ABC123&dn=foo"),
            magnet_item("magnet:?tr=udp://tracker&xt=urn:btih:abc123"),
            magnet_item("   "),
        ]);

        assert_eq!(empty_num, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].magnet, "magnet:?xt=urn:btih:abc123");
    }

    #[test]
    fn test_find_blacklist_candidate_matches_regex_rule() {
        let mut item = magnet_item("magnet:?xt=urn:btih:abc123");
        item.title = "My Anime FIN".to_string();

        let matched = find_blacklist_candidate(&[item.clone()], Some("/(?i)(fin|end)/")).unwrap();
        assert_eq!(matched.unwrap().title, item.title);
    }

    #[test]
    fn test_filter_blacklisted_configs_skips_matching_rss_url() {
        let blacklist = BlacklistService::new_in_memory(6).unwrap();
        let item = MagnetItem {
            title: "合集".to_string(),
            link: "https://example.com/item".to_string(),
            magnet: "magnet:?xt=urn:btih:abc123".to_string(),
            description: None,
            content: None,
        };
        blacklist
            .blacklist_rss(
                "https://example.com/rss?subgroupid=12&bangumiId=2739",
                &item,
            )
            .unwrap();

        let filtered = filter_blacklisted_configs(
            &blacklist,
            vec![
                named_config("https://example.com/rss?bangumiId=2739&subgroupid=12"),
                named_config("https://example.com/rss?bangumiId=8888"),
            ],
        )
        .unwrap();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].url, "https://example.com/rss?bangumiId=8888");
    }

    #[tokio::test]
    async fn test_should_skip_rss_error_only_for_retry_exhausted_fetch_failures() {
        let retry_exhausted: anyhow::Error = crate::rss_site::RssFetchError::new(
            "https://example.com/rss",
            3,
            crate::rss_site::RssFetchStage::Body,
            true,
            anyhow!("timeout"),
        )
        .into();
        let regular = anyhow!("invalid regex filter");

        assert!(should_skip_rss_error(&retry_exhausted));
        assert!(!should_skip_rss_error(&regular));
    }
}
