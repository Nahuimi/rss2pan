use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rquest::{Method, RequestBuilder};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::time::sleep;

use crate::{
    m115::crypto,
    request::Ajax,
    utils::{open_qrcode_image, remove_qrcode_image, write_qrcode_image},
};

const API_ADD_OFFLINE_URL: &str = "https://lixian.115.com/lixianssp/?ac=add_task_urls";
const API_CLEAR_OFFLINE_URL: &str = "https://lixian.115.com/lixian/?ct=lixian&ac=task_clear";
const API_STATUS_CHECK: &str = "https://my.115.com/?ct=guide&ac=status";
const API_USER_INFO: &str = "https://my.115.com/?ct=ajax&ac=nav";
const API_QRCODE_TOKEN_URL: &str = "https://qrcodeapi.115.com/api/1.0/%s/1.0/token";
const API_QRCODE_STATUS_URL: &str = "https://qrcodeapi.115.com/get/status/";
const API_QRCODE_LOGIN_URL: &str = "https://passportapi.115.com/app/1.0/%s/1.0/login/qrcode";
const API_QRCODE_IMAGE_URL: &str =
    "https://qrcodeapi.115.com/api/1.0/web/1.0/qrcode?qrfrom=1&client=0&uid=%s";
const APP_VER: &str = "27.0.5.7";
const UA_115_BROWSER: &str = "Mozilla/5.0 115Browser/27.0.5.7";
const REQUIRED_COOKIE_NAMES: [&str; 3] = ["UID", "CID", "SEID"];
const QRCODE_LOGIN_TIMEOUT: Duration = Duration::from_secs(120);
const QRCODE_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
struct QrcodeEnvelope<T> {
    state: i64,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    errno: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct QrcodeToken {
    uid: String,
    time: i64,
    sign: String,
}

#[derive(Debug, Deserialize)]
struct QrcodeStatus {
    status: i64,
}

#[derive(Debug, Deserialize)]
struct QrcodeCookie {
    #[serde(rename = "UID")]
    uid: String,
    #[serde(rename = "CID")]
    cid: String,
    #[serde(rename = "SEID")]
    seid: String,
    #[serde(rename = "KID", default)]
    kid: String,
}

#[derive(Debug, Deserialize)]
struct QrcodeLogin {
    cookie: QrcodeCookie,
    user_id: i64,
    #[allow(dead_code)]
    user_name: String,
}

#[derive(Debug)]
struct QrcodeSession {
    app: String,
    uid: String,
    time: i64,
    sign: String,
    image: Vec<u8>,
}

#[derive(Debug)]
struct QrcodeImageGuard {
    path: PathBuf,
}

impl QrcodeImageGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for QrcodeImageGuard {
    fn drop(&mut self) {
        remove_qrcode_image(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pan115ErrorKind {
    NotLogin,
    OfflineNoTimes,
    InvalidLink,
    TaskExisted,
    Exist,
    Unexpected,
}

#[derive(Debug)]
pub struct Pan115Error {
    kind: Pan115ErrorKind,
    code: i64,
    message: Option<String>,
}

impl Pan115Error {
    pub(crate) fn new(code: i64, message: Option<String>) -> Self {
        Self {
            kind: match code {
                99 | 990001 => Pan115ErrorKind::NotLogin,
                10010 => Pan115ErrorKind::OfflineNoTimes,
                10004 => Pan115ErrorKind::InvalidLink,
                10008 => Pan115ErrorKind::TaskExisted,
                20004 => Pan115ErrorKind::Exist,
                _ => Pan115ErrorKind::Unexpected,
            },
            code,
            message,
        }
    }

    pub fn kind(&self) -> Pan115ErrorKind {
        self.kind
    }
}

impl std::fmt::Display for Pan115Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self.kind {
            Pan115ErrorKind::NotLogin => "115 need login",
            Pan115ErrorKind::OfflineNoTimes => "offline download quota used up",
            Pan115ErrorKind::InvalidLink => "invalid download link",
            Pan115ErrorKind::TaskExisted => "offline task existed",
            Pan115ErrorKind::Exist => "target already exists",
            Pan115ErrorKind::Unexpected => "unexpected 115 error",
        };
        match self.message.as_deref() {
            Some(message) if !message.is_empty() => {
                write!(f, "{label} (code {}): {message}", self.code)
            }
            _ => write!(f, "{label} (code {})", self.code),
        }
    }
}

impl std::error::Error for Pan115Error {}

#[derive(Clone)]
pub struct Pan115Client {
    ajax: Ajax,
    user_id: Arc<Mutex<Option<i64>>>,
}

impl Pan115Client {
    pub fn new(ajax: Ajax) -> Self {
        Self {
            ajax,
            user_id: Arc::new(Mutex::new(None)),
        }
    }

    fn req(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        Ok(self
            .ajax
            .gen_req_host(method, url, "115.com")?
            .header(rquest::header::USER_AGENT, UA_115_BROWSER))
    }

    fn qrcode_req(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        Ok(self
            .ajax
            .gen_req(method, url)?
            .header(rquest::header::USER_AGENT, UA_115_BROWSER))
    }

    async fn request_json(&self, request: RequestBuilder, endpoint: &str) -> Result<Value> {
        let response = request
            .send()
            .await
            .with_context(|| format!("request 115 api failed: {endpoint}"))?
            .error_for_status()
            .with_context(|| format!("115 api returned non-success status: {endpoint}"))?;
        let body = response
            .bytes()
            .await
            .with_context(|| format!("read 115 api response failed: {endpoint}"))?;
        parse_json_response_bytes(&body)
            .map_err(|err| anyhow!("decode 115 api response failed: {endpoint}; {err}"))
    }

    async fn request_qrcode<T>(&self, request: RequestBuilder, endpoint: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let value = self.request_json(request, endpoint).await?;
        let envelope: QrcodeEnvelope<T> = serde_json::from_value(value)
            .with_context(|| format!("decode qrcode api response failed: {endpoint}"))?;
        if envelope.state != 0 {
            return envelope
                .data
                .context(format!("missing qrcode api payload: {endpoint}"));
        }
        let code = envelope.code.or(envelope.errno).unwrap_or_default();
        Err(qrcode_api_error(code, envelope.message.or(envelope.error)))
    }

    pub async fn ensure_logged_in(&self) -> Result<()> {
        self.ensure_cookie_fields()?;
        if self.cookie_check().await? {
            Ok(())
        } else {
            bail!("115 need login")
        }
    }

    pub async fn login_with_qrcode(&self, app: &str) -> Result<()> {
        let session = self.start_qrcode_session(app).await?;
        let image_path = QrcodeImageGuard::new(write_qrcode_image(&session.image)?);
        println!("QR code image saved to {}", image_path.path().display());
        if let Err(err) = open_qrcode_image(image_path.path()) {
            log::warn!("open qrcode image failed: {err}");
            println!(
                "Open {} manually and scan it with the 115 app",
                image_path.path().display()
            );
        }

        let deadline = tokio::time::Instant::now() + QRCODE_LOGIN_TIMEOUT;
        let mut scanned = false;
        let login = loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("login timed out");
            }

            match self.poll_qrcode_status(&session).await? {
                0 => {}
                1 => {
                    if !scanned {
                        println!("QR code scanned, confirm login in the 115 app");
                        scanned = true;
                    }
                }
                2 => break self.finish_qrcode_login(&session).await?,
                -2 => {
                    bail!("login cancelled");
                }
                status => {
                    bail!("unexpected qrcode status: {status}");
                }
            }

            sleep(QRCODE_POLL_INTERVAL).await;
        };

        self.apply_qrcode_login(&login)?;
        println!("115 login success");
        Ok(())
    }

    async fn start_qrcode_session(&self, app: &str) -> Result<QrcodeSession> {
        let token_url = qrcode_token_url(app);
        let token = self
            .request_qrcode::<QrcodeToken>(self.qrcode_req(Method::GET, &token_url)?, &token_url)
            .await?;
        let image_url = API_QRCODE_IMAGE_URL.replace("%s", &token.uid);
        let image = self
            .qrcode_req(Method::GET, &image_url)?
            .send()
            .await
            .with_context(|| format!("request qrcode image failed: {image_url}"))?
            .error_for_status()
            .with_context(|| format!("qrcode image returned non-success status: {image_url}"))?
            .bytes()
            .await
            .with_context(|| format!("read qrcode image failed: {image_url}"))?
            .to_vec();

        Ok(QrcodeSession {
            app: app.to_string(),
            uid: token.uid,
            time: token.time,
            sign: token.sign,
            image,
        })
    }

    async fn poll_qrcode_status(&self, session: &QrcodeSession) -> Result<i64> {
        let status = self
            .request_qrcode::<QrcodeStatus>(
                self.qrcode_req(Method::GET, API_QRCODE_STATUS_URL)?
                    .query(&[
                        ("uid", session.uid.as_str()),
                        ("time", &session.time.to_string()),
                        ("sign", session.sign.as_str()),
                        ("_", &now_millis().to_string()),
                    ]),
                API_QRCODE_STATUS_URL,
            )
            .await?;
        Ok(status.status)
    }

    async fn finish_qrcode_login(&self, session: &QrcodeSession) -> Result<QrcodeLogin> {
        let login_url = qrcode_login_url(&session.app);
        self.request_qrcode::<QrcodeLogin>(
            self.qrcode_req(Method::POST, &login_url)?.form(&[
                ("account", session.uid.as_str()),
                ("app", session.app.as_str()),
            ]),
            &login_url,
        )
        .await
    }

    fn apply_qrcode_login(&self, login: &QrcodeLogin) -> Result<()> {
        let cookie = format_cookie_string(
            &login.cookie.uid,
            &login.cookie.cid,
            &login.cookie.seid,
            Some(&login.cookie.kid),
        );
        self.ajax
            .set_cookie_for_host("115.com", Some(cookie.clone()));
        self.ajax.save_cookie_config("115.com", &cookie)?;
        *self.user_id.lock().unwrap() = Some(login.user_id);
        Ok(())
    }

    async fn cookie_check(&self) -> Result<bool> {
        let value = self
            .request_json(
                self.req(Method::GET, API_STATUS_CHECK)?
                    .query(&[("_", now_millis().to_string())]),
                API_STATUS_CHECK,
            )
            .await?;
        Ok(value.get("state").and_then(json_bool).unwrap_or(false))
    }

    async fn user_id(&self) -> Result<i64> {
        if let Some(user_id) = *self.user_id.lock().unwrap() {
            return Ok(user_id);
        }

        if let Some(cookie_uid) = self.cookie_uid() {
            *self.user_id.lock().unwrap() = Some(cookie_uid);
            return Ok(cookie_uid);
        }

        let value = self
            .request_json(
                self.req(Method::GET, API_USER_INFO)?
                    .query(&[("_", now_secs().to_string())]),
                API_USER_INFO,
            )
            .await?;
        ensure_success(&value)?;
        let user_id = value
            .get("data")
            .and_then(|data| data.get("user_id"))
            .and_then(json_i64)
            .context("missing user_id in 115 response")?;
        *self.user_id.lock().unwrap() = Some(user_id);
        Ok(user_id)
    }

    pub async fn clear_offline_tasks(&self, flag: u8) -> Result<()> {
        let value = self
            .request_json(
                self.req(Method::POST, API_CLEAR_OFFLINE_URL)?
                    .form(&[("flag", flag.to_string())]),
                API_CLEAR_OFFLINE_URL,
            )
            .await?;
        ensure_success(&value)?;
        Ok(())
    }

    pub async fn add_offline_urls(
        &self,
        uris: &[String],
        dir_id: Option<&str>,
        savepath: Option<&str>,
    ) -> Result<Vec<String>> {
        if uris.is_empty() {
            return Ok(vec![]);
        }

        let key = crypto::gen_key();
        let params =
            build_add_offline_params(uris, dir_id, savepath, self.user_id().await?.to_string());
        let params_json = serde_json::to_vec(&params)?;
        let encoded = crypto::encode(&params_json, &key);
        let value = self
            .request_json(
                self.req(Method::POST, API_ADD_OFFLINE_URL)?
                    .query(&[("t", now_secs().to_string())])
                    .form(&[("data", encoded)]),
                API_ADD_OFFLINE_URL,
            )
            .await?;
        ensure_success(&value)?;

        let encoded_data = value
            .get("data")
            .and_then(|value| value.as_str())
            .context("missing encrypted data in 115 response")?;
        let decoded = crypto::decode(encoded_data, &key)
            .map_err(|err| anyhow!("decode 115 offline response failed: {err}"))?;
        let decoded_value: Value = serde_json::from_slice(&decoded)?;
        let hashes = decoded_value
            .get("result")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("info_hash").and_then(|value| value.as_str()))
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(hashes)
    }

    fn ensure_cookie_fields(&self) -> Result<()> {
        let cookie = self.ajax.cookie_for_host("115.com").context(
            "115 cookies is required. Use --cookies, config.toml, or create a .cookies file",
        )?;
        let missing = REQUIRED_COOKIE_NAMES
            .iter()
            .copied()
            .filter(|name| cookie_value(&cookie, name).is_none())
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        bail!(
            "115 cookies format error, missing {}. Use --cookies \"UID=...;CID=...;SEID=...;KID=...\", config.toml, or create a .cookies file",
            missing.join(", ")
        )
    }

    fn cookie_uid(&self) -> Option<i64> {
        self.ajax
            .cookie_for_host("115.com")
            .and_then(|cookie| cookie_value(&cookie, "UID"))
            .and_then(|value| extract_cookie_uid(&value))
    }
}

fn build_add_offline_params(
    uris: &[String],
    dir_id: Option<&str>,
    savepath: Option<&str>,
    user_id: String,
) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    params.insert("ac".to_string(), "add_task_urls".to_string());
    params.insert("app_ver".to_string(), APP_VER.to_string());
    params.insert("uid".to_string(), user_id);

    if let Some(dir_id) = dir_id.map(str::trim).filter(|value| !value.is_empty()) {
        params.insert("wp_path_id".to_string(), dir_id.to_string());
    }
    if let Some(savepath) = savepath.map(str::trim).filter(|value| !value.is_empty()) {
        params.insert("savepath".to_string(), savepath.to_string());
    }
    for (index, uri) in uris.iter().enumerate() {
        params.insert(format!("url[{index}]"), uri.to_string());
    }

    params
}

fn ensure_success(value: &Value) -> Result<()> {
    let code = first_non_zero([
        value.get("errno"),
        value.get("errNo"),
        value.get("code"),
        value.get("err_code"),
    ])
    .unwrap_or_default();
    let state = value.get("state").and_then(json_bool).unwrap_or(code == 0);
    if state && code == 0 {
        return Ok(());
    }
    Err(Pan115Error::new(code, extract_message(value)).into())
}

fn extract_message(value: &Value) -> Option<String> {
    ["error_msg", "msg", "error", "message"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(|value| value.as_str()))
        .map(|value| value.to_string())
}

fn first_non_zero<const N: usize>(values: [Option<&Value>; N]) -> Option<i64> {
    values
        .into_iter()
        .flatten()
        .filter_map(json_i64)
        .find(|value| *value != 0)
}

fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        Value::Bool(flag) => Some(if *flag { 1 } else { 0 }),
        _ => None,
    }
}

fn json_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        Value::String(text) => match text.as_str() {
            "1" | "true" | "TRUE" => Some(true),
            "0" | "false" | "FALSE" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_json_response_bytes(body: &[u8]) -> Result<Value> {
    let trimmed = trim_response_bytes(body);
    if trimmed.is_empty() {
        bail!("empty response body");
    }

    if let Ok(value) = serde_json::from_slice(trimmed) {
        return Ok(value);
    }

    let body = String::from_utf8_lossy(trimmed);
    parse_json_response_text(&body)
}

fn parse_json_response_text(body: &str) -> Result<Value> {
    let trimmed = body.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        bail!("empty response body");
    }

    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }

    if is_abnormal_operation_response(trimmed) {
        bail!("115 abnormal operation");
    }

    if let Some(fragment) = extract_json_fragment(trimmed) {
        if let Ok(value) = serde_json::from_str(fragment) {
            return Ok(value);
        }
    }

    bail!("body preview: {}", preview_body(trimmed))
}

fn trim_response_bytes(body: &[u8]) -> &[u8] {
    let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(body);
    let start = body
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(body.len());
    let end = body
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &body[start..end]
}

fn is_abnormal_operation_response(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("abnormal operation")
        || body.contains("操作异常")
        || body.contains("异常验证")
        || body.contains("验证码")
}

fn extract_json_fragment(body: &str) -> Option<&str> {
    let start = body.find(['{', '['])?;
    let end = body.rfind(['}', ']'])?;
    if end <= start {
        return None;
    }
    Some(body[start..=end].trim())
}

fn preview_body(body: &str) -> String {
    const LIMIT: usize = 240;

    let normalized = body.replace(['\r', '\n'], " ");
    let mut preview: String = normalized.chars().take(LIMIT).collect();
    if normalized.chars().count() > LIMIT {
        preview.push_str("...");
    }
    preview
}

fn qrcode_api_error(code: i64, message: Option<String>) -> anyhow::Error {
    let label = match code {
        40199002 => "qrcode expired",
        50199004 => "get qrcode token failed",
        _ => "qrcode login failed",
    };
    match message.as_deref() {
        Some(message) if !message.is_empty() => anyhow!("{label} (code {code}): {message}"),
        _ => anyhow!("{label} (code {code})"),
    }
}

fn qrcode_token_url(app: &str) -> String {
    API_QRCODE_TOKEN_URL.replace("%s", app)
}

fn qrcode_login_url(app: &str) -> String {
    API_QRCODE_LOGIN_URL.replace("%s", app)
}

fn format_cookie_string(uid: &str, cid: &str, seid: &str, kid: Option<&str>) -> String {
    let mut parts = vec![
        format!("UID={uid}"),
        format!("CID={cid}"),
        format!("SEID={seid}"),
    ];
    if let Some(kid) = kid.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(format!("KID={kid}"));
    }
    parts.join("; ")
}

fn cookie_value(cookie: &str, name: &str) -> Option<String> {
    cookie
        .split(';')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .find_map(|segment| {
            let (key, value) = segment.split_once('=')?;
            if key.trim().eq_ignore_ascii_case(name) {
                let value = value.trim();
                if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                }
            } else {
                None
            }
        })
}

fn extract_cookie_uid(value: &str) -> Option<i64> {
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_helpers() {
        assert_eq!(json_i64(&Value::String("12".to_string())), Some(12));
        assert_eq!(json_bool(&Value::String("true".to_string())), Some(true));
    }

    #[test]
    fn test_pan115_error_kind() {
        assert_eq!(
            Pan115Error::new(10008, None).kind(),
            Pan115ErrorKind::TaskExisted
        );
        assert_eq!(Pan115Error::new(99, None).kind(), Pan115ErrorKind::NotLogin);
    }

    #[test]
    fn test_parse_json_response_text_accepts_wrapped_json() {
        let value = parse_json_response_text("callback({\"state\":true,\"errno\":0});").unwrap();
        assert_eq!(value.get("state").and_then(json_bool), Some(true));
    }

    #[test]
    fn test_parse_json_response_text_accepts_lossy_text() {
        let raw = b"{\"state\":false,\"error\":\"\x80\"}";
        let body = String::from_utf8_lossy(raw);
        let value = parse_json_response_text(&body).unwrap();
        assert_eq!(value.get("state").and_then(json_bool), Some(false));
        assert!(value
            .get("error")
            .and_then(|value| value.as_str())
            .is_some());
    }

    #[test]
    fn test_parse_json_response_bytes_accepts_utf8_bom() {
        let value = parse_json_response_bytes(b"\xef\xbb\xbf{\"state\":true}").unwrap();
        assert_eq!(value.get("state").and_then(json_bool), Some(true));
    }

    #[test]
    fn test_cookie_value_case_insensitive() {
        let cookie = "uid=1; CID=2; seid=3; KID=4";
        assert_eq!(cookie_value(cookie, "UID").as_deref(), Some("1"));
        assert_eq!(cookie_value(cookie, "CID").as_deref(), Some("2"));
        assert_eq!(cookie_value(cookie, "SEID").as_deref(), Some("3"));
        assert_eq!(cookie_value(cookie, "KID").as_deref(), Some("4"));
    }

    #[test]
    fn test_build_add_offline_params_matches_python_behavior() {
        let params = build_add_offline_params(
            &[String::from("magnet:?xt=urn:btih:hash-a")],
            Some("123"),
            Some("桜都字幕组"),
            "1".to_string(),
        );
        assert_eq!(params.get("wp_path_id").map(String::as_str), Some("123"));
        assert_eq!(
            params.get("savepath").map(String::as_str),
            Some("桜都字幕组")
        );
        assert_eq!(params.get("uid").map(String::as_str), Some("1"));
        assert_eq!(
            params.get("url[0]").map(String::as_str),
            Some("magnet:?xt=urn:btih:hash-a")
        );
    }

    #[test]
    fn test_extract_cookie_uid_from_prefixed_numeric_uid() {
        assert_eq!(
            extract_cookie_uid("34352253467_D1_17225283483"),
            Some(34352253467)
        );
    }

    #[test]
    fn test_qrcode_envelope_success() {
        let value = serde_json::json!({
            "state": 1,
            "data": {
                "uid": "uid",
                "time": 1,
                "sign": "sign"
            }
        });
        let envelope: QrcodeEnvelope<QrcodeToken> = serde_json::from_value(value).unwrap();
        assert_eq!(envelope.data.unwrap().uid, "uid");
    }

    #[test]
    fn test_qrcode_api_error_label() {
        assert_eq!(
            qrcode_api_error(40199002, None).to_string(),
            "qrcode expired (code 40199002)"
        );
    }

    #[test]
    fn test_qrcode_urls_follow_selected_app() {
        assert_eq!(
            qrcode_token_url("ios"),
            "https://qrcodeapi.115.com/api/1.0/ios/1.0/token"
        );
        assert_eq!(
            qrcode_login_url("115android"),
            "https://passportapi.115.com/app/1.0/115android/1.0/login/qrcode"
        );
    }

    #[test]
    fn test_format_cookie_string_skips_empty_kid() {
        assert_eq!(
            format_cookie_string("1", "2", "3", Some("")),
            "UID=1; CID=2; SEID=3"
        );
    }

    #[test]
    fn test_qrcode_image_guard_removes_file_on_drop() {
        let path = PathBuf::from(format!(
            "qrcode115-test-{}-{}.png",
            std::process::id(),
            now_millis()
        ));
        std::fs::write(&path, b"test").unwrap();
        {
            let guard = QrcodeImageGuard::new(path.clone());
            assert_eq!(guard.path(), path.as_path());
            assert!(path.exists());
        }
        assert!(!path.exists());
    }
}
