use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{anyhow, Context, Result};
use clap::ArgMatches;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, RequestBuilder,
};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SiteConfig {
    #[serde(rename = "httpsAgent")]
    pub https_agent: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

pub type NodeSiteConfig = HashMap<String, SiteConfig>;

fn load_cookie_file() -> Option<String> {
    fs::read_to_string(".cookies")
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn build_proxy_client() -> Result<Client> {
    let proxy_url = env::var("ALL_PROXY")
        .or_else(|_| env::var("HTTPS_PROXY"))
        .unwrap_or_else(|_| "http://127.0.0.1:10808".to_string());
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    let proxy = reqwest::Proxy::all(&proxy_url)
        .with_context(|| format!("invalid proxy URL in ALL_PROXY/HTTPS_PROXY: {proxy_url}"))?;
    reqwest::ClientBuilder::new()
        .user_agent(ua)
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(20))
        .proxy(proxy)
        .build()
        .context("build proxy client failed")
}

pub fn build_client() -> Client {
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    reqwest::ClientBuilder::new()
        .user_agent(ua)
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap()
}

#[derive(Clone)]
pub struct Ajax {
    inner_client: reqwest::Client,
    inner_client_proxy: Option<reqwest::Client>,
    proxy_client_error: Option<String>,
    site_config: NodeSiteConfig,
    cookie_override: Option<String>,
}

fn get_site_config(filename: Option<PathBuf>) -> NodeSiteConfig {
    let path = match filename {
        Some(path) => Some(path),
        None => {
            let local = PathBuf::from("node-site-config.json");
            if local.exists() {
                Some(local)
            } else {
                dirs::home_dir().map(|home| home.join("node-site-config.json"))
            }
        }
    };
    let Some(path) = path else {
        return NodeSiteConfig::default();
    };
    if !Path::new(&path).exists() {
        return NodeSiteConfig::default();
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

impl Ajax {
    pub fn new(cookie_override: Option<String>) -> Self {
        let (inner_client_proxy, proxy_client_error) = match build_proxy_client() {
            Ok(client) => (Some(client), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Self {
            inner_client: build_client(),
            inner_client_proxy,
            proxy_client_error,
            site_config: get_site_config(None),
            cookie_override,
        }
    }

    pub fn from_matches(matches: &ArgMatches) -> Self {
        Self::new(matches.get_one::<String>("cookies").cloned())
    }

    fn resolved_cookie(&self, host: &str, config: Option<&SiteConfig>) -> Option<String> {
        let config_cookie = config.and_then(|cur| {
            cur.headers
                .get("cookie")
                .cloned()
                .or_else(|| cur.headers.get("Cookie").cloned())
        });
        if host.contains("115.com") {
            self.cookie_override
                .clone()
                .or(config_cookie)
                .or_else(load_cookie_file)
        } else {
            config_cookie
        }
    }

    pub fn cookie_for_host(&self, host: &str) -> Option<String> {
        let config = self.site_config.get(host);
        self.resolved_cookie(host, config)
    }

    fn build_headers(&self, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let config = self.site_config.get(host);
        if let Some(config) = config {
            for (key, value) in &config.headers {
                if key.eq_ignore_ascii_case("cookie") {
                    continue;
                }
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_str(key.as_str()),
                    HeaderValue::from_str(value.as_str()),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        if let Some(cookie) = self.resolved_cookie(host, config) {
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                headers.insert(reqwest::header::COOKIE, value);
            }
        }
        headers
    }

    pub fn gen_req(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .unwrap_or_default();
        self.gen_req_host(method, url, &host)
    }

    pub fn gen_req_host(&self, method: Method, url: &str, host: &str) -> Result<RequestBuilder> {
        let headers = self.build_headers(host);
        let use_proxy = self
            .site_config
            .get(host)
            .and_then(|config| config.https_agent.as_ref())
            .is_some();
        if use_proxy {
            let client = self.inner_client_proxy.as_ref().ok_or_else(|| {
                anyhow!(
                    "proxy client is unavailable for host {host}: {}",
                    self.proxy_client_error
                        .as_deref()
                        .unwrap_or("unknown proxy configuration error")
                )
            })?;
            Ok(client.request(method, url).headers(headers))
        } else {
            Ok(self.inner_client.request(method, url).headers(headers))
        }
    }
}
