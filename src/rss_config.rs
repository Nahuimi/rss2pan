use std::{collections::HashMap, fs::File, io::BufReader, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RssConfig {
    pub name: String,
    pub url: String,
    pub cid: Option<String>,
    pub savepath: Option<String>,
    pub filter: Option<String>,
    pub expiration: Option<u8>,
}

pub fn get_rss_dict(path: Option<&PathBuf>) -> anyhow::Result<HashMap<String, Vec<RssConfig>>> {
    let file = if let Some(path) = path {
        File::open(path)?
    } else {
        File::open("rss.json")?
    };
    let reader = BufReader::new(file);
    let rss_dict: HashMap<String, Vec<RssConfig>> = serde_json::from_reader(reader)?;
    Ok(rss_dict)
}

pub fn get_rss_config_by_url(path: Option<&PathBuf>, url: &str) -> anyhow::Result<RssConfig> {
    let rss_dict = match get_rss_dict(path) {
        Ok(rss_dict) => rss_dict,
        Err(_) => {
            return Ok(RssConfig {
                url: url.to_string(),
                ..RssConfig::default()
            })
        }
    };
    let site = url::Url::parse(url)?
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("invalid rss url: {url}"))?
        .to_string();
    let config = rss_dict.get(&site).and_then(|configs| {
        configs
            .iter()
            .find(|config| is_same_rss_url(&config.url, url))
    });
    Ok(match config {
        Some(config) => config.clone(),
        None => RssConfig {
            url: url.to_string(),
            ..RssConfig::default()
        },
    })
}

fn is_same_rss_url(left: &str, right: &str) -> bool {
    left == right || normalize_rss_url(left) == normalize_rss_url(right)
}

fn normalize_rss_url(raw: &str) -> String {
    let Ok(url) = url::Url::parse(raw) else {
        return raw.to_string();
    };

    let mut host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if let Some(port) = url.port() {
        host = format!("{host}:{port}");
    }

    let path = url.path().trim_end_matches('/');
    let mut params = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    params.sort_unstable();

    let query = params
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() {
        format!("{host}{path}")
    } else {
        format!("{host}{path}?{query}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_url_with_reordered_query() {
        assert!(is_same_rss_url(
            "https://example.com/rss?a=1&b=2",
            "https://example.com/rss?b=2&a=1"
        ));
    }
}
