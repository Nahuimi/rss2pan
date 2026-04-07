use std::{fs, path::PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

const RSS_FILE_NAME: &str = "rss.json";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RssConfig {
    pub name: String,
    pub url: String,
    pub cid: Option<String>,
    pub savepath: Option<String>,
    pub filter: Option<String>,
    pub expiration: Option<u8>,
}

fn default_rss_path() -> PathBuf {
    PathBuf::from(RSS_FILE_NAME)
}

fn resolve_rss_path(path: Option<&PathBuf>) -> PathBuf {
    path.cloned().unwrap_or_else(default_rss_path)
}

pub fn get_rss_list(path: Option<&PathBuf>) -> anyhow::Result<Vec<RssConfig>> {
    let path = resolve_rss_path(path);
    let content =
        fs::read_to_string(&path).with_context(|| format!("read {} failed", path.display()))?;
    let raw: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parse {} failed", path.display()))?;

    if raw.is_object() {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(RSS_FILE_NAME);
        bail!("{file_name} format changed: use a flat array instead of host-keyed object");
    }
    if !raw.is_array() {
        bail!(
            "parse {} failed: rss.json root must be an array",
            path.display()
        );
    }

    serde_json::from_value(raw).with_context(|| format!("parse {} failed", path.display()))
}

pub fn get_rss_config_by_url(path: Option<&PathBuf>, url: &str) -> anyhow::Result<RssConfig> {
    let path = resolve_rss_path(path);
    if !path.exists() {
        return Ok(default_rss_config(url));
    }

    let config = get_rss_list(Some(&path))?
        .into_iter()
        .find(|config| is_same_rss_url(&config.url, url));

    Ok(config.unwrap_or_else(|| default_rss_config(url)))
}

fn default_rss_config(url: &str) -> RssConfig {
    RssConfig {
        url: url.to_string(),
        ..RssConfig::default()
    }
}

fn is_same_rss_url(left: &str, right: &str) -> bool {
    left == right || normalize_rss_url(left) == normalize_rss_url(right)
}

pub(crate) fn normalize_rss_url(raw: &str) -> String {
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
    use std::{env, fs};

    fn temp_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "rss2pan-rss-json-{}-{}-{}.json",
            name,
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    fn remove_temp_file(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_same_url_with_reordered_query() {
        assert!(is_same_rss_url(
            "https://example.com/rss?a=1&b=2",
            "https://example.com/rss?b=2&a=1"
        ));
    }

    #[test]
    fn test_get_rss_config_by_url_matches_flat_list_with_reordered_query() {
        let rss_path = temp_path("flat-list");
        fs::write(
            &rss_path,
            r#"[
  {
    "name": "test",
    "url": "https://mikanime.tv/RSS/Bangumi?subgroupid=12&bangumiId=2739"
  }
]"#,
        )
        .unwrap();

        let config = get_rss_config_by_url(
            Some(&rss_path),
            "https://mikanime.tv/RSS/Bangumi?bangumiId=2739&subgroupid=12",
        )
        .unwrap();
        assert_eq!(config.name, "test");

        remove_temp_file(&rss_path);
    }

    #[test]
    fn test_get_rss_config_by_url_returns_default_when_file_missing() {
        let rss_path = temp_path("missing");
        let config = get_rss_config_by_url(Some(&rss_path), "https://example.com/rss").unwrap();

        assert_eq!(config.url, "https://example.com/rss");
        assert!(config.name.is_empty());
        assert!(config.cid.is_none());
    }

    #[test]
    fn test_get_rss_list_rejects_legacy_host_keyed_object() {
        let rss_path = temp_path("legacy-object");
        fs::write(
            &rss_path,
            r#"{
  "mikanani.me": [
    {
      "name": "test",
      "url": "https://mikanani.me/RSS/Bangumi?bangumiId=2739&subgroupid=12"
    }
  ]
}"#,
        )
        .unwrap();

        let err = get_rss_list(Some(&rss_path)).unwrap_err();
        assert!(err
            .to_string()
            .contains("format changed: use a flat array instead of host-keyed object"));

        remove_temp_file(&rss_path);
    }
}
