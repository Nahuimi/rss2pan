use std::{collections::HashSet, path::PathBuf, time::Duration};

use anyhow::Result;
use clap::ArgMatches;
use futures::{stream, StreamExt, TryStreamExt};
use tokio::time::sleep;

use crate::{
    db::RssService,
    pan115::{Pan115Client, Pan115Error, Pan115ErrorKind},
    request::Ajax,
    rss_config::{get_rss_config_by_url, get_rss_dict, RssConfig},
    rss_site::{get_magnetitem_list, MagnetItem},
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

    pub async fn execute_url(&self, service: &mut RssService, url: &str) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        let config = get_rss_config_by_url(self.rss_path.as_ref(), url)?;
        let item_list = get_magnetitem_list(&self.ajax, &config).await?;
        self.execute_task(service, &item_list, &config).await
    }

    pub async fn execute_all(&self, service: &mut RssService) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        let rss_dict = get_rss_dict(self.rss_path.as_ref())?;
        for configs in rss_dict.values() {
            for config in configs {
                let item_list = get_magnetitem_list(&self.ajax, config).await?;
                self.execute_task(service, &item_list, config).await?;
            }
        }
        Ok(())
    }

    pub async fn execute_all_concurrent(&self, service: &mut RssService) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        let rss_dict = get_rss_dict(self.rss_path.as_ref())?;
        let configs = rss_dict
            .values()
            .flat_map(|configs| configs.iter().cloned())
            .collect::<Vec<_>>();

        let mut stream = stream::iter(configs.into_iter().map(|config| {
            let ajax = self.ajax.clone();
            async move {
                let item_list = get_magnetitem_list(&ajax, &config).await?;
                Ok::<_, anyhow::Error>((config, item_list))
            }
        }))
        .buffer_unordered(RSS_FETCH_CONCURRENCY);

        while let Some((config, item_list)) = stream.try_next().await? {
            self.execute_task(service, &item_list, &config).await?;
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
        item_list: &[MagnetItem],
        config: &RssConfig,
    ) -> Result<()> {
        let (deduped, empty_num) = dedup_task_items(item_list);

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

    fn magnet_item(magnet: &str) -> MagnetItem {
        MagnetItem {
            title: "title".to_string(),
            link: "link".to_string(),
            magnet: magnet.to_string(),
            description: None,
            content: None,
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
}
