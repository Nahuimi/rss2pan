use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::ArgMatches;
use futures::{stream, StreamExt};
use regex::Regex;
use tokio::time::sleep;

use crate::{
    db::{BlacklistService, RssService, StoredRssItem},
    pan115::{OfflineTask, Pan115Client, Pan115Error, Pan115ErrorKind},
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
    rss_paths: Option<Vec<PathBuf>>,
    options: RunOptions,
}

impl TaskRunner {
    pub fn new(
        pan115: Pan115Client,
        ajax: Ajax,
        rss_paths: Option<Vec<PathBuf>>,
        options: RunOptions,
    ) -> Self {
        Self {
            pan115,
            ajax,
            rss_paths,
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
        let config = get_rss_config_by_url(self.rss_paths.as_deref(), url)?;
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
        let configs =
            filter_blacklisted_configs(blacklist, get_rss_list(self.rss_paths.as_deref())?)?;
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
        let configs =
            filter_blacklisted_configs(blacklist, get_rss_list(self.rss_paths.as_deref())?)?;
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

    /// 把 115 上失败的离线任务删掉再重新提交。
    ///
    /// 连接在 `rss_items` 里有记录时，用数据库里保存的 `cid` / `savepath` 重新提交；
    /// 数据库里没有记录时，用 115 任务列表返回的 `wp_path_id` 作为 `cid`。
    pub async fn retry_failed_tasks(&self, service: &RssService) -> Result<RetryOutcome> {
        self.pan115.ensure_logged_in().await?;
        let tasks = self.pan115.list_offline_tasks().await?;
        let failed = collect_failed_tasks(tasks);
        if failed.is_empty() {
            log::info!("[retry] no failed 115 offline task");
            return Ok(RetryOutcome::default());
        }

        let magnets = failed
            .iter()
            .map(|task| canonicalize_magnet(&task.url))
            .collect::<Vec<_>>();
        let stored = service.find_items_by_magnets(&magnets)?;
        let plans = build_retry_plans(&failed, &stored);

        let mut outcome = RetryOutcome {
            failed_tasks: failed.len(),
            from_database: plans.iter().filter(|plan| plan.from_database).count(),
            from_api: plans.iter().filter(|plan| !plan.from_database).count(),
            ..RetryOutcome::default()
        };
        log::info!(
            "[retry] {} failed task(s): {} use db.sqlite, {} use 115 api",
            outcome.failed_tasks,
            outcome.from_database,
            outcome.from_api
        );

        let hashes = failed
            .iter()
            .map(|task| task.info_hash.clone())
            .collect::<Vec<_>>();
        for chunk in hashes.chunks(self.options.chunk_size) {
            self.pan115.delete_offline_tasks(chunk, false).await?;
            outcome.deleted_tasks += chunk.len();
            log::info!("[retry] deleted {} failed task(s)", chunk.len());
            sleep(self.options.chunk_delay).await;
        }

        for ((cid, savepath), group) in group_retry_plans(&plans) {
            for chunk in group.chunks(self.options.chunk_size) {
                match submit_links_chunk(&self.pan115, chunk, cid.as_deref(), savepath.as_deref())
                    .await
                {
                    Ok(_) => {
                        outcome.resubmitted += chunk.len();
                        log::info!(
                            "[retry] resubmitted {} task(s) with cid={:?} savepath={:?}",
                            chunk.len(),
                            cid,
                            savepath
                        );
                    }
                    Err(err) => {
                        outcome.resubmit_failures += chunk.len();
                        log::warn!("[retry] resubmit failed: {err}");
                    }
                }
                sleep(self.options.chunk_delay).await;
            }
        }

        Ok(outcome)
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
                    service.save_items(
                        chunk,
                        true,
                        config.cid.as_deref(),
                        config.savepath.as_deref(),
                    )?;
                }
                Ok(SubmitChunkOutcome::TaskExisted) => {
                    log::warn!("[{}] task exist", config_name_or_url(config));
                    service.save_items(
                        chunk,
                        true,
                        config.cid.as_deref(),
                        config.savepath.as_deref(),
                    )?;
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

/// 重试失败离线任务的结果统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetryOutcome {
    pub failed_tasks: usize,
    pub deleted_tasks: usize,
    pub resubmitted: usize,
    pub from_database: usize,
    pub from_api: usize,
    pub resubmit_failures: usize,
}

impl RetryOutcome {
    pub fn is_success(&self) -> bool {
        self.resubmit_failures == 0
    }
}

/// 一条失败任务重新提交时的参数来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryTaskPlan {
    pub magnet: String,
    pub cid: Option<String>,
    pub savepath: Option<String>,
    pub from_database: bool,
}

/// 只保留下载失败、`info_hash` 有效、并且连接可用的任务，按 `info_hash` 去重。
fn collect_failed_tasks(tasks: Vec<OfflineTask>) -> Vec<OfflineTask> {
    let mut seen = HashSet::new();
    tasks
        .into_iter()
        .filter(|task| task.is_failed())
        .filter(|task| !task.info_hash.is_empty() && !canonicalize_magnet(&task.url).is_empty())
        .filter(|task| seen.insert(task.info_hash.clone()))
        .collect()
}

/// 数据库里有这条连接时用数据库里的 `cid` / `savepath`，否则用 115 返回的 `wp_path_id`。
fn build_retry_plans(
    tasks: &[OfflineTask],
    stored: &HashMap<String, StoredRssItem>,
) -> Vec<RetryTaskPlan> {
    tasks
        .iter()
        .map(|task| {
            let magnet = canonicalize_magnet(&task.url);
            match stored.get(&magnet) {
                Some(item) => RetryTaskPlan {
                    cid: item.cid.clone().or_else(|| task.wp_path_id.clone()),
                    savepath: item.savepath.clone(),
                    from_database: true,
                    magnet,
                },
                None => RetryTaskPlan {
                    cid: task.wp_path_id.clone(),
                    savepath: None,
                    from_database: false,
                    magnet,
                },
            }
        })
        .collect()
}

/// 按 `cid` / `savepath` 分组，同一组可以一次提交多条连接。
fn group_retry_plans(
    plans: &[RetryTaskPlan],
) -> BTreeMap<(Option<String>, Option<String>), Vec<String>> {
    let mut groups = BTreeMap::new();
    for plan in plans {
        groups
            .entry((plan.cid.clone(), plan.savepath.clone()))
            .or_insert_with(Vec::new)
            .push(plan.magnet.clone());
    }
    groups
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

    fn offline_task(hash: &str, url: &str, status: i64, cid: Option<&str>) -> OfflineTask {
        OfflineTask {
            info_hash: hash.to_string(),
            name: "name".to_string(),
            url: url.to_string(),
            status,
            wp_path_id: cid.map(str::to_string),
        }
    }

    fn stored_item(cid: Option<&str>, savepath: Option<&str>) -> StoredRssItem {
        StoredRssItem {
            link: "link".to_string(),
            title: "title".to_string(),
            cid: cid.map(str::to_string),
            savepath: savepath.map(str::to_string),
        }
    }

    #[test]
    fn test_collect_failed_tasks_keeps_only_failed_valid_tasks() {
        let tasks = vec![
            offline_task("a", "magnet:?xt=urn:btih:aaa", -1, None),
            offline_task("b", "magnet:?xt=urn:btih:bbb", 2, None),
            offline_task("a", "magnet:?xt=urn:btih:aaa", -1, None),
            offline_task("c", "", -1, None),
            offline_task("", "magnet:?xt=urn:btih:ccc", -1, None),
        ];

        let failed = collect_failed_tasks(tasks);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].info_hash, "a");
    }

    #[test]
    fn test_build_retry_plans_prefers_database_cid_and_savepath() {
        let tasks = vec![
            offline_task("a", "magnet:?xt=urn:btih:aaa&dn=test", -1, Some("999")),
            offline_task("b", "magnet:?xt=urn:btih:bbb", -1, Some("888")),
            offline_task("c", "magnet:?xt=urn:btih:ccc", -1, Some("777")),
            offline_task("d", "magnet:?xt=urn:btih:ddd", -1, Some("666")),
        ];
        let stored = HashMap::from([
            (
                "magnet:?xt=urn:btih:aaa".to_string(),
                stored_item(Some("111"), Some("番剧/测试")),
            ),
            (
                "magnet:?xt=urn:btih:bbb".to_string(),
                stored_item(Some("222"), None),
            ),
            (
                "magnet:?xt=urn:btih:ccc".to_string(),
                stored_item(None, None),
            ),
        ]);

        let plans = build_retry_plans(&tasks, &stored);
        assert_eq!(plans.len(), 4);

        // 数据库里有 cid，优先用数据库的 cid 和 savepath
        assert_eq!(plans[0].cid.as_deref(), Some("111"));
        assert_eq!(plans[0].savepath.as_deref(), Some("番剧/测试"));
        assert_eq!(plans[0].magnet, "magnet:?xt=urn:btih:aaa");
        assert!(plans[0].from_database);

        assert_eq!(plans[1].cid.as_deref(), Some("222"));
        assert_eq!(plans[1].savepath, None);
        assert!(plans[1].from_database);

        // 数据库里有记录但没存 cid，退回 115 返回的 wp_path_id
        assert_eq!(plans[2].cid.as_deref(), Some("777"));
        assert!(plans[2].from_database);

        // 数据库里没有记录，用 115 返回的 wp_path_id
        assert_eq!(plans[3].cid.as_deref(), Some("666"));
        assert_eq!(plans[3].savepath, None);
        assert!(!plans[3].from_database);
    }

    #[test]
    fn test_group_retry_plans_groups_by_cid_and_savepath() {
        let plans = vec![
            RetryTaskPlan {
                magnet: "a".to_string(),
                cid: Some("1".to_string()),
                savepath: Some("x".to_string()),
                from_database: true,
            },
            RetryTaskPlan {
                magnet: "b".to_string(),
                cid: Some("1".to_string()),
                savepath: Some("x".to_string()),
                from_database: true,
            },
            RetryTaskPlan {
                magnet: "c".to_string(),
                cid: None,
                savepath: None,
                from_database: false,
            },
        ];

        let groups = group_retry_plans(&plans);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[&(Some("1".to_string()), Some("x".to_string()))],
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(groups[&(None, None)], vec!["c".to_string()]);
    }

    #[test]
    fn test_retry_outcome_is_success_only_without_failures() {
        assert!(RetryOutcome::default().is_success());
        assert!(!RetryOutcome {
            resubmit_failures: 1,
            ..RetryOutcome::default()
        }
        .is_success());
    }
}
