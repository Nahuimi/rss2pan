mod acgnx;
mod dmhy;
mod mikanani;
mod nyaa;
mod rsshub;

use anyhow::{anyhow, Context};
use regex::Regex;
use reqwest::Method;
use rss::{Channel, Item};
use std::io::BufReader;
use std::{fs::File, path::PathBuf};

pub use acgnx::*;
pub use dmhy::*;
pub use mikanani::*;
pub use nyaa::*;
pub use rsshub::*;

use crate::{request::Ajax, rss_config::RssConfig};

pub trait MagnetSite {
    fn get_magnet(&self, item: &Item) -> Option<String>;

    fn get_magnet_item(&self, item: &Item) -> Option<MagnetItem> {
        Some(MagnetItem {
            title: item.title().map_or_else(String::new, |s| s.to_string()),
            link: item.link().map_or_else(String::new, |s| s.to_string()),
            magnet: self.get_magnet(item)?,
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

pub fn get_site(name: &str) -> Option<Box<dyn MagnetSite>> {
    let site = if name.starts_with("http") {
        url::Url::parse(name)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .unwrap_or_else(|| name.to_string())
    } else {
        name.to_string()
    };

    match site.as_str() {
        "mikanani.me" | "mikanime.tv" => Some(Box::new(Mikanani)),
        "nyaa.si" | "sukebei.nyaa.si" => Some(Box::new(Nyaa)),
        "share.dmhy.org" => Some(Box::new(Dmhy)),
        "share.acgnx.se" | "www.acgnx.se" | "share.acgnx.net" => Some(Box::new(Acgnx)),
        "rsshub.app" => Some(Box::new(Rsshub)),
        _ => None,
    }
}

pub async fn get_feed(ajax: &Ajax, url: &str) -> anyhow::Result<Channel> {
    let content = ajax
        .gen_req(Method::GET, url)?
        .send()
        .await
        .with_context(|| format!("request rss failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("rss returned non-success status: {url}"))?
        .bytes()
        .await
        .with_context(|| format!("read rss body failed: {url}"))?;
    let channel = Channel::read_from(&content[..])?;
    Ok(channel)
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
    let Some(site) = get_site(&config.url) else {
        return Err(anyhow!("not support site: {}", config.url));
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

    #[test]
    fn test_db_save_items() {
        let channel = get_feed_by_file("tests/Bangumi.rss".into());
        assert!(channel.is_ok());
        let channel = channel.unwrap();

        let service = RssService::new().unwrap();
        let site = get_site("mikanani.me").unwrap();
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
        let site = get_site("share.acgnx.net").unwrap();
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
