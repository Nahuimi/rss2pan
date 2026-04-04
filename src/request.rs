use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::ArgMatches;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Method, RequestBuilder,
};
use serde::{Deserialize, Serialize};

const CONFIG_FILE_NAME: &str = "config.toml";
const DEFAULT_PROXY_ADDRESS: &str = "http://127.0.0.1:10808";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const SUPPORTED_TEMPLATE_PARSERS: &[&str] = &["acgnx", "dmhy", "mikanani", "nyaa", "rsshub"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProxyConfig {
    pub address: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_PROXY_ADDRESS.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TemplateConfig {
    pub domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proxy: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub cookies: BTreeMap<String, String>,
    pub template: BTreeMap<String, TemplateConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut cookies = BTreeMap::new();
        cookies.insert("115.com".to_string(), String::new());

        let mut template = BTreeMap::new();
        template.insert(
            "mikanani".to_string(),
            template_config(["mikanani.me", "mikanime.tv"], ["mikanani.me"]),
        );
        template.insert(
            "nyaa".to_string(),
            template_config(
                ["nyaa.si", "sukebei.nyaa.si"],
                ["nyaa.si", "sukebei.nyaa.si"],
            ),
        );
        template.insert(
            "dmhy".to_string(),
            template_config(["share.dmhy.org"], ["share.dmhy.org"]),
        );
        template.insert(
            "acgnx".to_string(),
            template_config(["share.acgnx.se", "www.acgnx.se", "share.acgnx.net"], []),
        );
        template.insert("rsshub".to_string(), template_config(["rsshub.app"], []));

        Self {
            proxy: ProxyConfig::default(),
            cookies,
            template,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSiteConfig {
    pub host: String,
    pub parser: Option<String>,
    pub use_proxy: bool,
}

fn template_config<const N: usize, const M: usize>(
    domains: [&str; N],
    proxy: [&str; M],
) -> TemplateConfig {
    TemplateConfig {
        domains: domains.into_iter().map(str::to_string).collect(),
        proxy: proxy.into_iter().map(str::to_string).collect(),
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

fn default_config_path() -> PathBuf {
    PathBuf::from(CONFIG_FILE_NAME)
}

pub fn default_config_toml() -> Result<String> {
    toml::to_string_pretty(&AppConfig::default()).context("serialize default config.toml failed")
}

pub fn ensure_default_config_file() -> Result<()> {
    let path = default_config_path();
    if path.exists() {
        return Ok(());
    }

    fs::write(&path, default_config_toml()?)
        .with_context(|| format!("write {} failed", path.display()))
}

fn validate_legacy_config_shape(raw: &toml::Value) -> Result<()> {
    if raw.get("sites").is_some() {
        bail!("config.toml format changed: use [template.<parser>] instead of [sites.\"host\"]");
    }
    if raw
        .get("proxy")
        .and_then(|value| value.get("domains"))
        .is_some()
    {
        bail!("config.toml format changed: move proxy.domains into template.<parser>.proxy");
    }
    if let Some(templates) = raw.get("template").and_then(|value| value.as_table()) {
        for (name, value) in templates {
            if value.get("parser").is_some() {
                bail!(
                    "config.toml format changed: remove template.{name}.parser and rename the table to [template.<parser>], for example [template.mikanani]"
                );
            }
            if value.get("rss_key").is_some() {
                bail!(
                    "config.toml format changed: remove template.{name}.rss_key; rss.json is now a flat array"
                );
            }
        }
    }
    Ok(())
}

fn validate_app_config(config: &AppConfig) -> Result<()> {
    let mut seen_domains = BTreeMap::<String, String>::new();

    for (name, template) in &config.template {
        if !SUPPORTED_TEMPLATE_PARSERS.contains(&name.as_str()) {
            bail!(
                "template.{name} is not supported; use parser names as template names: {}",
                SUPPORTED_TEMPLATE_PARSERS.join(", ")
            );
        }
        if template.domains.is_empty() {
            bail!("template.{name}.domains must not be empty");
        }

        let mut domains = BTreeSet::new();
        for domain in &template.domains {
            let domain = normalize_host(domain);
            if domain.is_empty() {
                bail!("template.{name}.domains contains empty host");
            }
            if !domains.insert(domain.clone()) {
                bail!("template.{name}.domains contains duplicate host: {domain}");
            }
            if let Some(previous) = seen_domains.insert(domain.clone(), name.clone()) {
                bail!(
                    "domain {domain} is declared in both template.{previous} and template.{name}"
                );
            }
        }

        for proxy_host in &template.proxy {
            let proxy_host = normalize_host(proxy_host);
            if proxy_host.is_empty() {
                bail!("template.{name}.proxy contains empty host");
            }
            if !domains.contains(&proxy_host) {
                bail!(
                    "template.{name}.proxy host {proxy_host} is not listed in template.{name}.domains"
                );
            }
        }
    }

    Ok(())
}

fn load_app_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        let config = AppConfig::default();
        validate_app_config(&config)?;
        return Ok(config);
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("read {} failed", path.display()))?;
    let raw: toml::Value =
        toml::from_str(&content).with_context(|| format!("parse {} failed", path.display()))?;
    validate_legacy_config_shape(&raw)?;
    let config: AppConfig = raw
        .try_into()
        .with_context(|| format!("parse {} failed", path.display()))?;
    validate_app_config(&config)?;
    Ok(config)
}

fn load_cookie_file() -> Option<String> {
    fs::read_to_string(".cookies")
        .ok()
        .and_then(|content| normalize_cookie_string(&content))
}

pub fn normalize_cookie_string(raw: &str) -> Option<String> {
    let parts = raw
        .split(';')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .filter_map(|segment| {
            let (key, value) = segment.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                None
            } else {
                Some(format!("{key}={value}"))
            }
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

pub fn build_proxy_client(proxy_url: &str) -> Result<Client> {
    let proxy = reqwest::Proxy::all(proxy_url)
        .with_context(|| format!("invalid proxy URL in config.toml: {proxy_url}"))?;
    reqwest::ClientBuilder::new()
        .user_agent(USER_AGENT)
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(20))
        .proxy(proxy)
        .build()
        .context("build proxy client failed")
}

pub fn build_client() -> Client {
    reqwest::ClientBuilder::new()
        .user_agent(USER_AGENT)
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap()
}

fn find_template_config<'a>(
    templates: &'a BTreeMap<String, TemplateConfig>,
    host: &str,
) -> Option<(&'a str, &'a TemplateConfig)> {
    templates
        .iter()
        .find(|(_, template)| {
            template
                .domains
                .iter()
                .any(|domain| domain.eq_ignore_ascii_case(host))
        })
        .map(|(parser, template)| (parser.as_str(), template))
}

fn find_cookie<'a>(cookies: &'a BTreeMap<String, String>, host: &str) -> Option<&'a String> {
    cookies.get(host).or_else(|| {
        cookies
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(host))
            .map(|(_, value)| value)
    })
}

fn resolve_site_from_config(config: &AppConfig, host: &str) -> ResolvedSiteConfig {
    let host = normalize_host(host);
    let template = find_template_config(&config.template, &host);
    ResolvedSiteConfig {
        parser: template.map(|(parser, _)| parser.to_string()),
        use_proxy: template.is_some_and(|(_, template)| {
            template
                .proxy
                .iter()
                .any(|domain| domain.eq_ignore_ascii_case(&host))
        }),
        host,
    }
}

#[derive(Clone)]
pub struct Ajax {
    inner_client: reqwest::Client,
    inner_client_proxy: Option<reqwest::Client>,
    proxy_client_error: Option<String>,
    app_config: Arc<Mutex<AppConfig>>,
    config_path: Arc<PathBuf>,
    cookie_override: Arc<Mutex<Option<String>>>,
}

impl Ajax {
    pub fn new(cookie_override: Option<String>) -> Result<Self> {
        Self::with_config_path(cookie_override, default_config_path())
    }

    pub(crate) fn with_config_path(
        cookie_override: Option<String>,
        config_path: PathBuf,
    ) -> Result<Self> {
        let app_config = load_app_config(&config_path)?;
        let (inner_client_proxy, proxy_client_error) =
            match build_proxy_client(&app_config.proxy.address) {
                Ok(client) => (Some(client), None),
                Err(err) => (None, Some(err.to_string())),
            };
        Ok(Self {
            inner_client: build_client(),
            inner_client_proxy,
            proxy_client_error,
            app_config: Arc::new(Mutex::new(app_config)),
            config_path: Arc::new(config_path),
            cookie_override: Arc::new(Mutex::new(
                cookie_override.and_then(|value| normalize_cookie_string(&value)),
            )),
        })
    }

    pub fn from_matches(matches: &ArgMatches) -> Result<Self> {
        Self::new(matches.get_one::<String>("cookies").cloned())
    }

    pub fn resolve_site(&self, host: &str) -> ResolvedSiteConfig {
        let config = self.app_config.lock().unwrap().clone();
        resolve_site_from_config(&config, host)
    }

    pub fn resolve_site_by_url(&self, url: &str) -> Result<ResolvedSiteConfig> {
        let host = url::Url::parse(url)?
            .host_str()
            .ok_or_else(|| anyhow!("invalid url: {url}"))?
            .to_string();
        Ok(self.resolve_site(&host))
    }

    fn config_cookie(&self, host: &str) -> Option<String> {
        let config = self.app_config.lock().unwrap();
        find_cookie(&config.cookies, host).and_then(|cookie| normalize_cookie_string(cookie))
    }

    fn resolved_cookie(&self, host: &str) -> Option<String> {
        let host = host.to_ascii_lowercase();
        let config_cookie = self.config_cookie(&host);
        let cookie_override = self.cookie_override.lock().unwrap().clone();
        if host == "115.com" {
            cookie_override.or(config_cookie).or_else(load_cookie_file)
        } else {
            config_cookie
        }
    }

    pub fn cookie_for_host(&self, host: &str) -> Option<String> {
        self.resolved_cookie(host)
    }

    pub fn set_cookie_for_host(&self, host: &str, cookie: Option<String>) {
        if host.eq_ignore_ascii_case("115.com") {
            *self.cookie_override.lock().unwrap() =
                cookie.and_then(|value| normalize_cookie_string(&value));
        }
    }

    pub fn save_cookie_config(&self, host: &str, cookie: &str) -> Result<()> {
        let host = host.to_ascii_lowercase();
        let cookie = normalize_cookie_string(cookie).ok_or_else(|| anyhow!("cookie is empty"))?;
        let content = {
            let mut config = self.app_config.lock().unwrap();
            config.cookies.insert(host, cookie);
            toml::to_string_pretty(&*config).context("serialize config.toml failed")?
        };
        fs::write(self.config_path.as_ref(), content)
            .with_context(|| format!("write {} failed", self.config_path.display()))
    }

    fn build_headers(&self, resolved: &ResolvedSiteConfig) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if resolved.parser.as_deref() == Some("mikanani") {
            if let Ok(value) = HeaderValue::from_str(&format!("https://{}/", resolved.host)) {
                headers.insert(reqwest::header::REFERER, value);
            }
        }
        if let Some(cookie) = self.resolved_cookie(&resolved.host) {
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
        let resolved = self.resolve_site(host);
        let headers = self.build_headers(&resolved);
        if resolved.use_proxy {
            let client = self.inner_client_proxy.as_ref().ok_or_else(|| {
                anyhow!(
                    "proxy client is unavailable for host {}: {}",
                    resolved.host,
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

#[cfg(test)]
mod tests {
    use super::{default_config_toml, Ajax, AppConfig, ResolvedSiteConfig};
    use reqwest::Method;
    use std::{env, fs, path::PathBuf};

    fn temp_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "rss2pan-{}-{}-{}.toml",
            name,
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    fn write_temp_config(name: &str, content: &str) -> PathBuf {
        let path = temp_path(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn remove_temp_file(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_cookie_override_can_be_updated() {
        let path = write_temp_config("cookie-override", &default_config_toml().unwrap());
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();
        assert_eq!(ajax.cookie_for_host("115.com"), None);

        ajax.set_cookie_for_host("115.com", Some("UID=1; CID=2; SEID=3".to_string()));
        assert_eq!(
            ajax.cookie_for_host("115.com").as_deref(),
            Some("UID=1; CID=2; SEID=3")
        );

        let cloned = ajax.clone();
        cloned.set_cookie_for_host("115.com", Some("UID=4;CID=5;SEID=6;".to_string()));
        assert_eq!(
            ajax.cookie_for_host("115.com").as_deref(),
            Some("UID=4; CID=5; SEID=6")
        );

        remove_temp_file(&path);
    }

    #[test]
    fn test_resolve_site_uses_parser_from_template_name() {
        let path = write_temp_config("resolve-site", &default_config_toml().unwrap());
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();

        assert_eq!(
            ajax.resolve_site("mikanime.tv"),
            ResolvedSiteConfig {
                host: "mikanime.tv".to_string(),
                parser: Some("mikanani".to_string()),
                use_proxy: false,
            }
        );
        assert!(ajax.resolve_site("share.dmhy.org").use_proxy);

        remove_temp_file(&path);
    }

    #[test]
    fn test_template_parser_field_is_rejected() {
        let path = write_temp_config(
            "legacy-parser-field",
            r#"[proxy]
address = "http://127.0.0.1:10808"

[template.mikan]
parser = "mikanani"
domains = ["mikanani.me"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected legacy parser field config to fail"),
            Err(err) => assert!(err.to_string().contains(
                "remove template.mikan.parser and rename the table to [template.<parser>]"
            )),
        }

        remove_temp_file(&path);
    }

    #[test]
    fn test_template_rss_key_field_is_rejected() {
        let path = write_temp_config(
            "legacy-rss-key-field",
            r#"[proxy]
address = "http://127.0.0.1:10808"

[template.mikanani]
rss_key = "mikanani.me"
domains = ["mikanani.me"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected legacy rss_key field config to fail"),
            Err(err) => assert!(err
                .to_string()
                .contains("remove template.mikanani.rss_key; rss.json is now a flat array")),
        }

        remove_temp_file(&path);
    }

    #[test]
    fn test_template_name_must_be_supported_parser() {
        let path = write_temp_config(
            "unsupported-template-name",
            r#"[proxy]
address = "http://127.0.0.1:10808"

[template.mikan]
domains = ["mikanani.me"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected unsupported template name config to fail"),
            Err(err) => assert!(err
                .to_string()
                .contains("template.mikan is not supported; use parser names as template names")),
        }

        remove_temp_file(&path);
    }

    #[test]
    fn test_mikan_referer_is_derived_from_request_host() {
        let path = write_temp_config("mikan-referer", &default_config_toml().unwrap());
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();

        let mikanime = ajax
            .gen_req_host(
                Method::GET,
                "https://mikanime.tv/RSS/Bangumi",
                "mikanime.tv",
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            mikanime
                .headers()
                .get(reqwest::header::REFERER)
                .and_then(|value| value.to_str().ok()),
            Some("https://mikanime.tv/")
        );

        let mikanani = ajax
            .gen_req_host(
                Method::GET,
                "https://mikanani.me/RSS/Bangumi",
                "mikanani.me",
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            mikanani
                .headers()
                .get(reqwest::header::REFERER)
                .and_then(|value| value.to_str().ok()),
            Some("https://mikanani.me/")
        );

        remove_temp_file(&path);
    }

    #[test]
    fn test_duplicate_template_domain_is_rejected() {
        let path = write_temp_config(
            "duplicate-domain",
            r#"[proxy]
address = "http://127.0.0.1:10808"

[template.mikanani]
domains = ["mikanani.me"]

[template.nyaa]
domains = ["mikanani.me"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected duplicate domain config to fail"),
            Err(err) => assert!(err.to_string().contains(
                "domain mikanani.me is declared in both template.mikanani and template.nyaa"
            )),
        }

        remove_temp_file(&path);
    }

    #[test]
    fn test_proxy_domains_must_exist_in_template_domains() {
        let path = write_temp_config(
            "invalid-proxy-domain",
            r#"[proxy]
address = "http://127.0.0.1:10808"

[template.mikanani]
domains = ["mikanani.me"]
proxy = ["mikanime.tv"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected invalid proxy host config to fail"),
            Err(err) => assert!(err.to_string().contains(
                "template.mikanani.proxy host mikanime.tv is not listed in template.mikanani.domains"
            )),
        }

        remove_temp_file(&path);
    }

    #[test]
    fn test_normalize_cookie_formats() {
        assert_eq!(
            super::normalize_cookie_string("UID=115; CID=a1e; SEID=37d; KID=40b").as_deref(),
            Some("UID=115; CID=a1e; SEID=37d; KID=40b")
        );
        assert_eq!(
            super::normalize_cookie_string("UID=115;CID=a1e;SEID=37d;KID=40b;").as_deref(),
            Some("UID=115; CID=a1e; SEID=37d; KID=40b")
        );
    }

    #[test]
    fn test_save_cookie_config_persists_normalized_cookie() {
        let path = write_temp_config("save-cookie", &default_config_toml().unwrap());
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();

        ajax.save_cookie_config("115.com", "UID=1;CID=2;SEID=3;")
            .unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        let config: AppConfig = toml::from_str(&saved).unwrap();
        assert_eq!(
            config.cookies.get("115.com").map(|value| value.as_str()),
            Some("UID=1; CID=2; SEID=3")
        );

        remove_temp_file(&path);
    }
}
