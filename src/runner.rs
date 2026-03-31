use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use clap::ArgMatches;
use futures::future;
use tokio::time::sleep;

use crate::{
    db::RssService,
    pan115::{Pan115Client, Pan115Error, Pan115ErrorKind},
    request::Ajax,
    rss_config::{get_rss_config_by_url, get_rss_dict, RssConfig},
    rss_site::{get_magnetitem_list, MagnetItem},
};

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

    pub async fn execute_url(&self, service: &RssService, url: &str) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        let config = get_rss_config_by_url(self.rss_path.as_ref(), url)?;
        let item_list = get_magnetitem_list(&self.ajax, &config).await?;
        self.execute_task(service, &item_list, &config).await
    }

    pub async fn execute_all(&self, service: &RssService) -> Result<()> {
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

    pub async fn execute_all_concurrent(&self, service: &RssService) -> Result<()> {
        self.pan115.ensure_logged_in().await?;
        let rss_dict = get_rss_dict(self.rss_path.as_ref())?;
        let configs = rss_dict
            .values()
            .flat_map(|configs| configs.iter().cloned())
            .collect::<Vec<_>>();
        let task_list = future::join_all(configs.into_iter().map(|config| {
            let ajax = self.ajax.clone();
            async move {
                let item_list = get_magnetitem_list(&ajax, &config).await;
                (config, item_list)
            }
        }))
        .await;
        for (config, item_list) in task_list {
            let item_list = item_list?;
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
        self.submit_links("[magnet]", links, cid.as_deref(), savepath.as_deref())
            .await
    }

    pub async fn submit_links(
        &self,
        label: &str,
        links: &[String],
        cid: Option<&str>,
        savepath: Option<&str>,
    ) -> Result<()> {
        for (index, chunk) in links.chunks(self.options.chunk_size).enumerate() {
            let chunk_links = chunk.to_vec();
            match self
                .pan115
                .add_offline_urls(&chunk_links, cid, savepath)
                .await
            {
                Ok(_) => {
                    log::info!("{label} add {} tasks", chunk.len());
                }
                Err(err) => match add_error_kind(&err) {
                    Some(Pan115ErrorKind::TaskExisted) => {
                        log::warn!("{label} task exist");
                    }
                    Some(Pan115ErrorKind::InvalidLink) => {
                        log::warn!("{label} wrong links");
                    }
                    _ => return Err(err),
                },
            }
            if index + 1 < chunk_count(links.len(), self.options.chunk_size) {
                sleep(self.options.chunk_delay).await;
            }
        }
        Ok(())
    }

    async fn execute_task(
        &self,
        service: &RssService,
        item_list: &[MagnetItem],
        config: &RssConfig,
    ) -> Result<()> {
        let mut empty_num = 0usize;
        let mut tasks = Vec::new();
        for item in item_list {
            if item.magnet.is_empty() {
                empty_num += 1;
                continue;
            }
            if !self.options.disable_cache && service.has_item(&item.magnet)? {
                continue;
            }
            tasks.push(item.clone());
        }

        if empty_num > 0 {
            log::warn!("[{}] has {} empty tasks", config.name_or_url(), empty_num);
        }
        if tasks.is_empty() {
            log::info!("[{}] has 0 task", config.name_or_url());
            return Ok(());
        }

        for (index, chunk) in tasks.chunks(self.options.chunk_size).enumerate() {
            let links = chunk
                .iter()
                .map(|item| item.magnet.clone())
                .collect::<Vec<_>>();
            match self
                .pan115
                .add_offline_urls(&links, config.cid.as_deref(), config.savepath.as_deref())
                .await
            {
                Ok(_) => {
                    log::info!(
                        "[{}] [{}] add {} tasks",
                        config.name_or_url(),
                        config.url,
                        chunk.len()
                    );
                    service.save_items(chunk, true)?;
                }
                Err(err) => match add_error_kind(&err) {
                    Some(Pan115ErrorKind::TaskExisted) => {
                        log::warn!("[{}] task exist", config.name_or_url());
                        service.save_items(chunk, true)?;
                    }
                    Some(Pan115ErrorKind::InvalidLink) => {
                        log::warn!("[{}] wrong links", config.name_or_url());
                    }
                    _ => return Err(err),
                },
            }
            if index + 1 < chunk_count(tasks.len(), self.options.chunk_size) {
                sleep(self.options.chunk_delay).await;
            }
        }

        Ok(())
    }
}

fn add_error_kind(err: &anyhow::Error) -> Option<Pan115ErrorKind> {
    err.downcast_ref::<Pan115Error>().map(Pan115Error::kind)
}

fn chunk_count(len: usize, size: usize) -> usize {
    if len == 0 {
        0
    } else {
        (len - 1) / size + 1
    }
}

trait RssConfigExt {
    fn name_or_url(&self) -> &str;
}

impl RssConfigExt for RssConfig {
    fn name_or_url(&self) -> &str {
        if self.name.is_empty() {
            &self.url
        } else {
            &self.name
        }
    }
}
