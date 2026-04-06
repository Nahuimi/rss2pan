use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use wreq::{
    header::{HeaderMap, HeaderValue},
    Client, Method, RequestBuilder,
};

const CONFIG_FILE_NAME: &str = "config.toml";
const DEFAULT_PROXY_ADDRESS: &str = "http://127.0.0.1:10808";
const DEFAULT_DATABASE_PATH: &str = "db.sqlite";
const DEFAULT_RSS_PATH: &str = "rss.json";
const DEFAULT_LOG_LEVEL: &str = "info";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const SUPPORTED_TEMPLATE_PARSERS: &[&str] = &["acgnx", "dmhy", "mikanani", "nyaa", "rsshub"];
const SUPPORTED_LOG_LEVELS: &[&str] = &["off", "error", "warn", "info", "debug", "trace"];

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PathsConfig {
    pub database: String,
    pub rss: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            database: DEFAULT_DATABASE_PATH.to_string(),
            rss: DEFAULT_RSS_PATH.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_string(),
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
    pub paths: PathsConfig,
    pub log: LogConfig,
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
            paths: PathsConfig::default(),
            log: LogConfig::default(),
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

#[derive(Default)]
struct PathOverrides {
    database: Option<String>,
    rss: Option<String>,
}

fn prepare_config_content_for_parse(content: &str) -> Result<(String, PathOverrides)> {
    let mut normalized = String::new();
    let mut overrides = PathOverrides::default();
    let mut in_paths = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_paths = trimmed == "[paths]";
        }

        if in_paths {
            if let Some(path) = parse_path_override(line, "database")? {
                overrides.database = Some(path);
                normalized.push_str("database = \"__RSS2PAN_DATABASE_PATH__\"\n");
                continue;
            }
            if let Some(path) = parse_path_override(line, "rss")? {
                overrides.rss = Some(path);
                normalized.push_str("rss = \"__RSS2PAN_RSS_PATH__\"\n");
                continue;
            }
        }

        normalized.push_str(line);
        normalized.push('\n');
    }

    Ok((normalized, overrides))
}

fn parse_path_override(line: &str, key: &str) -> Result<Option<String>> {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix(key) else {
        return Ok(None);
    };
    let Some(rest) = rest.trim_start().strip_prefix('=') else {
        return Ok(None);
    };
    let rest = rest.trim_start();
    let Some((value, tail)) = parse_path_value(rest)? else {
        return Ok(None);
    };
    let tail = tail.trim_start();
    if !tail.is_empty() && !tail.starts_with('#') {
        bail!("invalid [paths].{key} value");
    }
    Ok(Some(value))
}

fn parse_path_value(input: &str) -> Result<Option<(String, &str)>> {
    match input.chars().next() {
        Some('"') => Ok(Some(parse_basic_path_value(input)?)),
        Some('\'') => Ok(Some(parse_literal_path_value(input)?)),
        _ => Ok(None),
    }
}

fn parse_basic_path_value(input: &str) -> Result<(String, &str)> {
    let mut backslashes = 0;
    for (index, ch) in input.char_indices().skip(1) {
        match ch {
            '\\' => backslashes += 1,
            '"' if backslashes % 2 == 0 => {
                let inner = &input[1..index];
                return Ok((decode_basic_path_value(inner), &input[index + 1..]));
            }
            _ => backslashes = 0,
        }
    }
    bail!("unterminated quoted path value")
}

fn parse_literal_path_value(input: &str) -> Result<(String, &str)> {
    let rest = &input[1..];
    let Some(end) = rest.find('\'') else {
        bail!("unterminated literal path value");
    };
    Ok((rest[..end].to_string(), &rest[end + 1..]))
}

fn decode_basic_path_value(inner: &str) -> String {
    if has_odd_backslash_run(inner) {
        return inner.to_string();
    }

    let mut decoded = String::with_capacity(inner.len());
    let mut run = 0;
    for ch in inner.chars() {
        if ch == '\\' {
            run += 1;
            continue;
        }
        for _ in 0..run / 2 {
            decoded.push('\\');
        }
        run = 0;
        decoded.push(ch);
    }
    for _ in 0..run / 2 {
        decoded.push('\\');
    }
    decoded
}

fn has_odd_backslash_run(inner: &str) -> bool {
    let mut run = 0;
    for ch in inner.chars() {
        if ch == '\\' {
            run += 1;
            continue;
        }
        if run % 2 == 1 {
            return true;
        }
        run = 0;
    }
    run % 2 == 1
}

fn compact_template_arrays(content: &str) -> String {
    let mut result = String::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed == "domains = [" || trimmed == "proxy = [" {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            let field = if trimmed.starts_with("domains") {
                "domains"
            } else {
                "proxy"
            };
            let mut values = Vec::new();
            for next_line in lines.by_ref() {
                let next_trimmed = next_line.trim();
                if next_trimmed == "]" {
                    break;
                }
                values.push(next_trimmed.trim_end_matches(',').to_string());
            }
            result.push_str(&format!("{indent}{field} = [{}]\n", values.join(", ")));
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }

    result
}

fn serialize_app_config(config: &AppConfig) -> Result<String> {
    let content = toml::to_string_pretty(config).context("serialize config.toml failed")?;
    Ok(compact_template_arrays(&content))
}

pub fn default_config_toml() -> Result<String> {
    serialize_app_config(&AppConfig::default())
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
    if !SUPPORTED_LOG_LEVELS.contains(&config.log.level.as_str()) {
        bail!(
            "log.level must be one of: {}",
            SUPPORTED_LOG_LEVELS.join(", ")
        );
    }

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
    let (content, path_overrides) = prepare_config_content_for_parse(&content)?;
    let raw: toml::Value =
        toml::from_str(&content).with_context(|| format!("parse {} failed", path.display()))?;
    validate_legacy_config_shape(&raw)?;
    let mut config: AppConfig = raw
        .try_into()
        .with_context(|| format!("parse {} failed", path.display()))?;
    if let Some(database) = path_overrides.database {
        config.paths.database = database;
    }
    if let Some(rss) = path_overrides.rss {
        config.paths.rss = rss;
    }
    validate_app_config(&config)?;
    Ok(config)
}

pub fn load_default_app_config() -> Result<AppConfig> {
    load_app_config(&default_config_path())
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

fn build_client_with_proxy(proxy_url: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(20));
    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(proxy_url);
    }
    builder.build().context("build client failed")
}

pub fn build_proxy_client(proxy_url: &str) -> Result<Client> {
    build_client_with_proxy(Some(proxy_url)).context("build proxy client failed")
}

fn build_rss_proxy_client(proxy_url: &str) -> Result<Client> {
    build_client_with_proxy(Some(proxy_url)).context("build rss proxy client failed")
}

pub fn build_client() -> Client {
    build_client_with_proxy(None).unwrap()
}

pub fn build_rss_client() -> Client {
    build_client_with_proxy(None).unwrap()
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
    inner_client: Client,
    inner_client_proxy: Option<Client>,
    proxy_client_error: Option<String>,
    rss_client: Client,
    rss_client_proxy: Option<Client>,
    rss_proxy_client_error: Option<String>,
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
        let (rss_client_proxy, rss_proxy_client_error) =
            match build_rss_proxy_client(&app_config.proxy.address) {
                Ok(client) => (Some(client), None),
                Err(err) => (None, Some(err.to_string())),
            };
        Ok(Self {
            inner_client: build_client(),
            inner_client_proxy,
            proxy_client_error,
            rss_client: build_rss_client(),
            rss_client_proxy,
            rss_proxy_client_error,
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

    pub fn app_config(&self) -> AppConfig {
        self.app_config.lock().unwrap().clone()
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(self.app_config().paths.database)
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
            serialize_app_config(&config)?
        };
        fs::write(self.config_path.as_ref(), content)
            .with_context(|| format!("write {} failed", self.config_path.display()))
    }

    fn build_headers(&self, resolved: &ResolvedSiteConfig) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if resolved.parser.as_deref() == Some("mikanani") {
            if let Ok(value) = HeaderValue::from_str(&format!("https://{}/", resolved.host)) {
                headers.insert(wreq::header::REFERER, value);
            }
        }
        if let Some(cookie) = self.resolved_cookie(&resolved.host) {
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                headers.insert(wreq::header::COOKIE, value);
            }
        }
        headers
    }

    fn build_rss_headers(&self, resolved: &ResolvedSiteConfig) -> HeaderMap {
        let mut headers = self.build_headers(resolved);
        headers.insert(
            wreq::header::ACCEPT,
            HeaderValue::from_static(
                "application/rss+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.7",
            ),
        );
        headers.insert(
            wreq::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
        );
        headers.insert(
            wreq::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
        headers.insert(wreq::header::PRAGMA, HeaderValue::from_static("no-cache"));
        headers
    }

    pub fn gen_req(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .unwrap_or_default();
        self.gen_req_host(method, url, &host)
    }

    pub fn gen_rss_req(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .unwrap_or_default();
        let resolved = self.resolve_site(&host);
        let headers = self.build_rss_headers(&resolved);
        if resolved.use_proxy {
            let client = self.rss_client_proxy.as_ref().ok_or_else(|| {
                anyhow!(
                    "rss proxy client is unavailable for host {}: {}",
                    resolved.host,
                    self.rss_proxy_client_error
                        .as_deref()
                        .unwrap_or("unknown proxy configuration error")
                )
            })?;
            Ok(client.request(method, url).headers(headers))
        } else {
            Ok(self.rss_client.request(method, url).headers(headers))
        }
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
    use std::{env, fs, path::PathBuf};
    use wreq::Method;

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
    fn test_default_config_includes_paths_and_log() {
        let content = default_config_toml().unwrap();
        assert!(content.contains("[paths]"));
        assert!(content.contains("database = \"db.sqlite\""));
        assert!(content.contains("rss = \"rss.json\""));
        assert!(content.contains("[log]"));
        assert!(content.contains("level = \"info\""));
    }

    #[test]
    fn test_default_config_compacts_template_arrays() {
        let content = default_config_toml().unwrap();
        assert!(content
            .contains("domains = [\"share.acgnx.se\", \"www.acgnx.se\", \"share.acgnx.net\"]"));
        assert!(!content.contains("domains = [\n    \"share.acgnx.se\","));
    }

    #[test]
    fn test_windows_path_with_unescaped_backslashes_is_accepted() {
        let path = write_temp_config(
            "windows-unescaped-path",
            "[paths]\ndatabase = \"D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\db.sqlite\"\nrss = \"D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\rss.json\"\n\n[template.mikanani]\ndomains = [\"mikanani.me\"]\n",
        );
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();
        let config = ajax.app_config();

        assert_eq!(
            config.paths.database,
            "D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\db.sqlite"
        );
        assert_eq!(
            config.paths.rss,
            "D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\rss.json"
        );

        remove_temp_file(&path);
    }

    #[test]
    fn test_windows_path_with_literal_string_is_accepted() {
        let path = write_temp_config(
            "windows-literal-path",
            "[paths]\ndatabase = 'D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\db.sqlite'\n\n[template.mikanani]\ndomains = [\"mikanani.me\"]\n",
        );
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();

        assert_eq!(
            ajax.app_config().paths.database,
            "D:\\ruanjian\\WingetUI-data\\winget\\rss2pan\\123\\db.sqlite"
        );

        remove_temp_file(&path);
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
    fn test_invalid_log_level_is_rejected() {
        let path = write_temp_config(
            "invalid-log-level",
            r#"[log]
level = "verbose"

[template.mikanani]
domains = ["mikanani.me"]
"#,
        );

        match Ajax::with_config_path(None, path.clone()) {
            Ok(_) => panic!("expected invalid log level config to fail"),
            Err(err) => assert!(err
                .to_string()
                .contains("log.level must be one of: off, error, warn, info, debug, trace")),
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
                .get(wreq::header::REFERER)
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
                .get(wreq::header::REFERER)
                .and_then(|value| value.to_str().ok()),
            Some("https://mikanani.me/")
        );

        remove_temp_file(&path);
    }

    #[test]
    fn test_gen_rss_req_adds_rss_headers() {
        let path = write_temp_config("rss-headers", &default_config_toml().unwrap());
        let ajax = Ajax::with_config_path(None, path.clone()).unwrap();

        let request = ajax
            .gen_rss_req(Method::GET, "https://mikanime.tv/RSS/Bangumi")
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get(wreq::header::REFERER)
                .and_then(|value| value.to_str().ok()),
            Some("https://mikanime.tv/")
        );
        assert_eq!(
            request
                .headers()
                .get(wreq::header::ACCEPT)
                .and_then(|value| value.to_str().ok()),
            Some("application/rss+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.7")
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
    fn test_save_cookie_config_persists_normalized_cookie_and_compacts_arrays() {
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
        assert!(
            saved.contains("domains = [\"share.acgnx.se\", \"www.acgnx.se\", \"share.acgnx.net\"]")
        );

        remove_temp_file(&path);
    }
}
