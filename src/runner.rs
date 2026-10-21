use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::ArgMatches;
use futures::{stream, StreamExt};
use regex::Regex;
use tokio::time::sleep;

use crate::{
    db::{BlacklistService, RssService, StoredRssItem},
    failed_tasks::{self, FailedSubmit},
    pan115::{OfflineTask, Pan115Client, Pan115Error, Pan115ErrorKind},
    request::Ajax,
    rss_config::{get_rss_config_by_url, get_rss_list, RssConfig},
    rss_site::{get_magnetitem_list, is_retry_exhausted_rss_error, MagnetItem},
    utils::{canonicalize_magnet, current_beijing_rfc3339},
};

const RSS_FETCH_CONCURRENCY: usize = 4;
/// 115 单次 `add_task_urls` 最多接受的链接数，超过要分批次提交。
pub const MAX_LINKS_PER_SUBMIT: usize = crate::pan115::MAX_ADD_LINKS_PER_REQUEST;
/// 提交失败后的重试间隔。115 偶尔会瞬时失败；任务刚删除时也可能暂时报“任务已存在”。
const SUBMIT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(6)];
/// 调用 115 的重试接口 / 删除任务之后，等 115 处理完再去看结果的时间。
const RETRY_SETTLE_DELAY: Duration = Duration::from_secs(3);

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
            chunk_size: MAX_LINKS_PER_SUBMIT,
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
            options.chunk_size = chunk_size.clamp(1, MAX_LINKS_PER_SUBMIT);
        }
        if let Some(chunk_delay) = matches.get_one::<u64>("chunk-delay").copied() {
            options.chunk_delay = Duration::from_secs(chunk_delay);
        }
        options
    }

    /// 实际提交时每批的链接数，永远不超过 115。
    pub fn chunk_size(&self) -> usize {
        self.chunk_size.clamp(1, MAX_LINKS_PER_SUBMIT)
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

    /// 重试 115 上失败的离线任务。
    ///
    /// 一次拉取全部离线任务，再用 `task_clear(flag=2)` 一次性清掉 115 上所有失败任务
    /// （等价于 `--clear-task-type 3`），最后用数据库里的 `cid` / `savepath` 重新提交。
    pub async fn retry_failed_tasks(&self, service: &RssService) -> Result<RetryOutcome> {
        self.pan115.ensure_logged_in().await?;
        let failed = collect_failed_tasks(self.pan115.list_offline_tasks().await?);
        if failed.is_empty() {
            log::info!("[retry] no failed 115 offline task");
            return Ok(RetryOutcome::default());
        }

        let mut outcome = RetryOutcome {
            failed_tasks: failed.len(),
            ..RetryOutcome::default()
        };
        log::info!("[retry] {} failed task(s)", outcome.failed_tasks);

        // 1. 从 db.sqlite 里找出这些连接用的 cid / savepath。
        let magnets = failed
            .iter()
            .map(|task| canonicalize_magnet(&task.url))
            .collect::<Vec<_>>();
        let stored = service.find_items_by_magnets(&magnets)?;
        let plans = build_retry_plans(&failed, &stored);
        outcome.from_database = plans.iter().filter(|plan| plan.from_database).count();
        outcome.from_api = plans.iter().filter(|plan| !plan.from_database).count();
        log::info!(
            "[retry] {} use db.sqlite, {} use 115 api",
            outcome.from_database,
            outcome.from_api
        );

        // 2. 一次性清掉 115 上所有失败任务；失败时退回逐个删除。
        let hashes = failed
            .iter()
            .map(|task| task.info_hash.clone())
            .collect::<Vec<_>>();
        let hash_set = hashes.iter().map(String::as_str).collect::<HashSet<_>>();
        match self.pan115.clear_failed_offline_tasks().await {
            Ok(()) => {
                outcome.deleted_tasks = failed.len();
                log::info!(
                    "[retry] cleared {} failed task(s) with task_clear(flag=2)",
                    outcome.deleted_tasks
                );
            }
            Err(err) => {
                log::warn!(
                    "[retry] clear failed task(s) failed ({err}), deleting one by one instead"
                );
                for chunk in hashes.chunks(self.options.chunk_size()) {
                    self.pan115.delete_offline_tasks(chunk, false).await?;
                    outcome.deleted_tasks += chunk.len();
                    log::info!("[retry] deleted {} failed task(s)", chunk.len());
                    sleep(self.options.chunk_delay).await;
                }
            }
        }

        // 3. 等 115 处理完删除，再确认一下失败任务是不是真的没了。
        sleep(RETRY_SETTLE_DELAY).await;
        match self.count_still_failed(&hash_set).await {
            Ok(count) => {
                outcome.still_failed = count;
                if count > 0 {
                    log::warn!(
                        "[retry] {count} failed task(s) are still in 115 after clearing, 稍后再试或先在 115 客户端里手动清除失败任务"
                    );
                }
            }
            Err(err) => {
                log::warn!("[retry] cannot check whether the failed task(s) are gone: {err}")
            }
        }

        // 4. 用 db.sqlite 里的 cid / savepath 重新提交。
        let mut failures = Vec::new();
        for ((cid, savepath), group) in group_retry_plans(&plans) {
            for chunk in group.chunks(self.options.chunk_size()) {
                match submit_chunk_with_retry(
                    &self.pan115,
                    chunk,
                    cid.as_deref(),
                    savepath.as_deref(),
                )
                .await
                {
                    Ok(SubmitChunkOutcome::Added) => {
                        outcome.resubmitted += chunk.len();
                        log::info!(
                            "[retry] resubmitted {} task(s) with cid={:?} savepath={:?}",
                            chunk.len(),
                            cid,
                            savepath
                        );
                    }
                    Ok(SubmitChunkOutcome::TaskExisted) => {
                        outcome.already_exists += chunk.len();
                        log::warn!(
                            "[retry] 115 still reports task existed for {} task(s) (cid={:?} savepath={:?}), 稍后可以再跑一次 --retry-failed",
                            chunk.len(),
                            cid,
                            savepath
                        );
                    }
                    Err(err) => {
                        outcome.resubmit_failures += chunk.len();
                        log::warn!("[retry] resubmit failed after retries: {err}");
                        failures.push(FailedSubmit::new(
                            "[retry]",
                            cid.clone(),
                            savepath.clone(),
                            format!("{err:#}"),
                            chunk.to_vec(),
                        ));
                    }
                }
                sleep(self.options.chunk_delay).await;
            }
        }

        if !failures.is_empty() {
            outcome.failed_json_path = Some(self.record_failed_submits(&failures)?);
        }

        Ok(outcome)
    }

    /// 重试 `failed_tasks.json` 里记录的失败链接。
    ///
    /// 提交成功（含 115 报“任务已存在”）的链接会从文件里删除；仍然失败的链接保留并更新错误信息。
    pub async fn retry_failed_json(&self, path: Option<PathBuf>) -> Result<RetryJsonOutcome> {
        let path =
            path.unwrap_or_else(|| failed_tasks::path_for_database(&self.pan115.database_path()));
        let mut file = failed_tasks::read(&path)?;
        let mut outcome = RetryJsonOutcome {
            file_path: path.clone(),
            entries: file.failures.len(),
            links: file.link_count(),
            ..RetryJsonOutcome::default()
        };
        if file.is_empty() {
            log::info!("[retry-json] no failed link in {}", path.display());
            return Ok(outcome);
        }

        self.pan115.ensure_logged_in().await?;
        log::info!(
            "[retry-json] retry {} failed link(s) from {} entry/entries in {}",
            outcome.links,
            outcome.entries,
            path.display()
        );

        let chunk_size = self.options.chunk_size();
        let mut remaining = Vec::new();
        let mut fatal = false;
        for failure in file.failures.drain(..) {
            if fatal {
                remaining.push(failure);
                continue;
            }

            let mut pending_links = Vec::new();
            let mut last_error = failure.error.clone();
            for chunk in failure.links.chunks(chunk_size) {
                if fatal {
                    pending_links.extend_from_slice(chunk);
                    continue;
                }
                match submit_chunk_with_retry(
                    &self.pan115,
                    chunk,
                    failure.cid.as_deref(),
                    failure.savepath.as_deref(),
                )
                .await
                {
                    Ok(SubmitChunkOutcome::Added) => {
                        outcome.resubmitted += chunk.len();
                        log::info!(
                            "[retry-json] resubmitted {} link(s) with cid={:?} savepath={:?}",
                            chunk.len(),
                            failure.cid,
                            failure.savepath
                        );
                    }
                    Ok(SubmitChunkOutcome::TaskExisted) => {
                        outcome.already_exists += chunk.len();
                        log::warn!(
                            "[retry-json] 115 still reports task existed for {} link(s) (cid={:?} savepath={:?}), treating as added",
                            chunk.len(),
                            failure.cid,
                            failure.savepath
                        );
                    }
                    Err(err) => {
                        last_error = format!("{err:#}");
                        pending_links.extend_from_slice(chunk);
                        log::warn!("[retry-json] submit failed after retries: {err}");
                        if is_fatal_submit_error(&err) {
                            fatal = true;
                        }
                    }
                }
                sleep(self.options.chunk_delay).await;
            }

            if !pending_links.is_empty() {
                remaining.push(FailedSubmit {
                    created_at: failure.created_at,
                    source: failure.source,
                    cid: failure.cid,
                    savepath: failure.savepath,
                    error: last_error,
                    links: pending_links,
                });
            }
        }

        file.updated_at = current_beijing_rfc3339();
        file.failures = remaining;
        failed_tasks::write(&path, &file)?;
        outcome.still_failed = file.link_count();
        outcome.remaining_entries = file.failures.len();
        log::info!(
            "[retry-json] done: {} resubmitted, {} already existed, {} still failed in {} entry/entries, saved to {}",
            outcome.resubmitted,
            outcome.already_exists,
            outcome.still_failed,
            outcome.remaining_entries,
            path.display()
        );
        if fatal {
            log::error!(
                "[retry-json] stopped retrying remaining entries after a fatal 115 error; they stay in {}",
                path.display()
            );
        }
        Ok(outcome)
    }

    /// 重新拉一次任务列表，数出 `hashes` 里仍然处于失败状态的任务数。
    async fn count_still_failed(&self, hashes: &HashSet<&str>) -> Result<usize> {
        let tasks = collect_failed_tasks(self.pan115.list_offline_tasks().await?);
        Ok(tasks
            .into_iter()
            .filter(|task| hashes.contains(task.info_hash.as_str()))
            .count())
    }

    /// 把失败的批次写进 `failed_tasks.json`，返回文件路径。
    fn record_failed_submits(&self, failures: &[FailedSubmit]) -> Result<PathBuf> {
        record_failed_submits(&self.pan115, failures)
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

        let chunk_size = self.options.chunk_size();
        let total_chunks = chunk_count(tasks.len(), chunk_size);
        let mut failures = Vec::new();
        for (index, chunk) in tasks.chunks(chunk_size).enumerate() {
            let links = chunk
                .iter()
                .map(|item| item.magnet.clone())
                .collect::<Vec<_>>();
            match submit_chunk_with_retry(
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
                Err(err) => {
                    log::warn!(
                        "[{}] add {} task(s) failed after retries: {err}",
                        config_name_or_url(config),
                        chunk.len()
                    );
                    failures.push(FailedSubmit::new(
                        config_name_or_url(config),
                        config.cid.clone(),
                        config.savepath.clone(),
                        format!("{err:#}"),
                        links,
                    ));
                    if is_fatal_submit_error(&err) {
                        log::error!(
                            "[{}] stop submitting remaining chunks: {err}",
                            config_name_or_url(config)
                        );
                        break;
                    }
                }
            }
            if index + 1 < total_chunks {
                sleep(self.options.chunk_delay).await;
            }
        }

        if !failures.is_empty() {
            let path = self.record_failed_submits(&failures)?;
            bail!(
                "[{}] {} batch(es) failed to submit, failed links saved to {}",
                config_name_or_url(config),
                failures.len(),
                path.display()
            );
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
    let chunk_size = options.chunk_size();
    let total_chunks = chunk_count(links.len(), chunk_size);
    let mut failures = Vec::new();
    for (index, chunk) in links.chunks(chunk_size).enumerate() {
        match submit_chunk_with_retry(pan115, chunk, cid, savepath).await {
            Ok(SubmitChunkOutcome::Added) => {
                log::info!("{label} add {} tasks", chunk.len());
            }
            Ok(SubmitChunkOutcome::TaskExisted) => {
                log::warn!("{label} task exist");
            }
            Err(err) => {
                log::warn!(
                    "{label} add {} task(s) failed after retries: {err}",
                    chunk.len()
                );
                failures.push(FailedSubmit::new(
                    label,
                    cid.map(str::to_string),
                    savepath.map(str::to_string),
                    format!("{err:#}"),
                    chunk.to_vec(),
                ));
                if is_fatal_submit_error(&err) {
                    log::error!("{label} stop submitting remaining chunks: {err}");
                    break;
                }
            }
        }
        if index + 1 < total_chunks {
            sleep(options.chunk_delay).await;
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        let path = record_failed_submits(pan115, &failures)?;
        bail!(
            "{label} {} batch(es) failed to submit, failed links saved to {}",
            failures.len(),
            path.display()
        );
    }
}

/// 把失败的批次写进 `failed_tasks.json`，返回文件路径。
fn record_failed_submits(pan115: &Pan115Client, failures: &[FailedSubmit]) -> Result<PathBuf> {
    let path = failed_tasks::path_for_database(&pan115.database_path());
    failed_tasks::append(&path, failures)?;
    log::error!(
        "[failed] {} batch(es) / {} link(s) failed to submit, saved to {}",
        failures.len(),
        failures
            .iter()
            .map(|failure| failure.links.len())
            .sum::<usize>(),
        path.display()
    );
    Ok(path)
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

/// 提交一批链接；瞬时失败和“任务已存在”会隔一会儿再重试。
async fn submit_chunk_with_retry(
    pan115: &Pan115Client,
    links: &[String],
    cid: Option<&str>,
    savepath: Option<&str>,
) -> Result<SubmitChunkOutcome> {
    let max_attempts = SUBMIT_RETRY_DELAYS.len() + 1;
    for attempt in 1..=max_attempts {
        match submit_links_chunk(pan115, links, cid, savepath).await {
            Ok(SubmitChunkOutcome::TaskExisted) if attempt < max_attempts => {
                let delay = SUBMIT_RETRY_DELAYS[attempt - 1];
                log::warn!(
                    "115 still reports task existed for {} task(s) on attempt {}/{}, retry in {:?}",
                    links.len(),
                    attempt,
                    max_attempts,
                    delay
                );
                sleep(delay).await;
            }
            Ok(outcome) => return Ok(outcome),
            Err(err) if attempt < max_attempts && should_retry_submit(&err) => {
                let delay = SUBMIT_RETRY_DELAYS[attempt - 1];
                log::warn!(
                    "submit {} task(s) failed on attempt {}/{}: {err}, retry in {:?}",
                    links.len(),
                    attempt,
                    max_attempts,
                    delay
                );
                sleep(delay).await;
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!()
}

/// 115 瞬时失败或网络错误可以再试一次；登录失效、没有离线次数、链接非法重试也没用。
fn should_retry_submit(err: &anyhow::Error) -> bool {
    matches!(
        add_error_kind(err),
        None | Some(Pan115ErrorKind::Unexpected)
    )
}

/// 登录失效、离线次数用完时继续提交剩下的批次没有意义。
fn is_fatal_submit_error(err: &anyhow::Error) -> bool {
    matches!(
        add_error_kind(err),
        Some(Pan115ErrorKind::NotLogin | Pan115ErrorKind::OfflineNoTimes)
    )
}

/// 重试失败离线任务的结果统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryOutcome {
    pub failed_tasks: usize,
    pub deleted_tasks: usize,
    /// 清掉失败任务之后，115 任务列表里仍然失败的、属于本次重试范围内的任务数。
    pub still_failed: usize,
    pub resubmitted: usize,
    /// 115 仍然认为任务存在、没有重新提交的任务数。
    pub already_exists: usize,
    pub from_database: usize,
    pub from_api: usize,
    pub resubmit_failures: usize,
    /// 仍有链接提交失败时，`failed_tasks.json` 的路径。
    pub failed_json_path: Option<PathBuf>,
}

impl RetryOutcome {
    pub fn is_success(&self) -> bool {
        self.resubmit_failures == 0
    }
}

/// 重试 `failed_tasks.json` 的结果统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryJsonOutcome {
    pub file_path: PathBuf,
    /// 文件里原本的记录条数和链接数。
    pub entries: usize,
    pub links: usize,
    pub resubmitted: usize,
    pub already_exists: usize,
    /// 重试后仍然失败、保留下来的链接数。
    pub still_failed: usize,
    pub remaining_entries: usize,
}

impl RetryJsonOutcome {
    pub fn is_success(&self) -> bool {
        self.still_failed == 0
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
    fn test_run_options_chunk_size_never_exceeds_115() {
        assert_eq!(RunOptions::default().chunk_size(), MAX_LINKS_PER_SUBMIT);
        assert_eq!(
            RunOptions {
                chunk_size: 500,
                ..RunOptions::default()
            }
            .chunk_size(),
            MAX_LINKS_PER_SUBMIT
        );
        assert_eq!(
            RunOptions {
                chunk_size: 0,
                ..RunOptions::default()
            }
            .chunk_size(),
            1
        );
        assert_eq!(
            RunOptions {
                chunk_size: 50,
                ..RunOptions::default()
            }
            .chunk_size(),
            50
        );
    }

    #[test]
    fn test_chunk_count_splits_more_than_115_links() {
        assert_eq!(chunk_count(115, MAX_LINKS_PER_SUBMIT), 1);
        assert_eq!(chunk_count(116, MAX_LINKS_PER_SUBMIT), 2);
        assert_eq!(chunk_count(215, MAX_LINKS_PER_SUBMIT), 2);
        assert_eq!(chunk_count(1000, MAX_LINKS_PER_SUBMIT), 9);
    }

    #[test]
    fn test_should_retry_submit_only_for_transient_errors() {
        assert!(should_retry_submit(&Pan115Error::new(99999, None).into()));
        assert!(should_retry_submit(&anyhow!("network reset")));
        assert!(!should_retry_submit(&Pan115Error::new(10004, None).into()));
        assert!(!should_retry_submit(&Pan115Error::new(10010, None).into()));
        assert!(!should_retry_submit(&Pan115Error::new(99, None).into()));
    }

    #[test]
    fn test_is_fatal_submit_error_only_for_login_and_quota() {
        assert!(is_fatal_submit_error(&Pan115Error::new(99, None).into()));
        assert!(is_fatal_submit_error(&Pan115Error::new(10010, None).into()));
        assert!(!is_fatal_submit_error(
            &Pan115Error::new(99999, None).into()
        ));
        assert!(!is_fatal_submit_error(&anyhow!("network reset")));
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

    #[test]
    fn test_retry_json_outcome_is_success_only_without_failures() {
        assert!(RetryJsonOutcome::default().is_success());
        assert!(!RetryJsonOutcome {
            still_failed: 1,
            ..RetryJsonOutcome::default()
        }
        .is_success());
    }

    #[test]
    fn test_submit_limit_constant_matches_115_api_limit() {
        assert_eq!(MAX_LINKS_PER_SUBMIT, 115);
        assert_eq!(
            MAX_LINKS_PER_SUBMIT,
            crate::pan115::MAX_ADD_LINKS_PER_REQUEST
        );
    }
}
