use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Method, RequestBuilder};
use serde_json::Value;

use crate::{m115::crypto, request::Ajax};

const API_ADD_OFFLINE_URL: &str = "https://lixian.115.com/lixianssp/?ac=add_task_urls";
const API_CLEAR_OFFLINE_URL: &str = "https://lixian.115.com/lixian/?ct=lixian&ac=task_clear";
const API_FILE_LIST: &str = "https://webapi.115.com/files";
const API_DIR_ADD: &str = "https://webapi.115.com/files/add";
const API_STATUS_CHECK: &str = "https://my.115.com/?ct=guide&ac=status";
const API_USER_INFO: &str = "https://my.115.com/?ct=ajax&ac=nav";
const APP_VER: &str = "27.0.5.7";
const UA_115_BROWSER: &str = "Mozilla/5.0 115Browser/27.0.5.7";
const MAX_DIR_PAGE_LIMIT: i64 = 1150;

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
    fn new(code: i64, message: Option<String>) -> Self {
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
            .header(reqwest::header::USER_AGENT, UA_115_BROWSER))
    }

    async fn request_json(&self, request: RequestBuilder, endpoint: &str) -> Result<Value> {
        let response = request
            .send()
            .await
            .with_context(|| format!("request 115 api failed: {endpoint}"))?
            .error_for_status()
            .with_context(|| format!("115 api returned non-success status: {endpoint}"))?;
        response
            .json()
            .await
            .with_context(|| format!("decode 115 api response failed: {endpoint}"))
    }

    pub async fn ensure_logged_in(&self) -> Result<()> {
        if self.cookie_check().await? {
            Ok(())
        } else {
            bail!("115 need login")
        }
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
    ) -> Result<Vec<String>> {
        if uris.is_empty() {
            return Ok(vec![]);
        }

        let key = crypto::gen_key();
        let mut params = BTreeMap::new();
        params.insert("ac".to_string(), "add_task_urls".to_string());
        params.insert(
            "wp_path_id".to_string(),
            dir_id.unwrap_or_default().to_string(),
        );
        params.insert("app_ver".to_string(), APP_VER.to_string());
        params.insert("uid".to_string(), self.user_id().await?.to_string());
        for (index, uri) in uris.iter().enumerate() {
            params.insert(format!("url[{index}]"), uri.to_string());
        }

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

    pub async fn resolve_target_dir(
        &self,
        cid: Option<&str>,
        savepath: Option<&str>,
    ) -> Result<Option<String>> {
        let cid = cid.map(str::trim).filter(|value| !value.is_empty());
        let savepath = savepath
            .map(|value| value.replace('\\', "/"))
            .and_then(|value| {
                let trimmed = value.trim().trim_matches('/').to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            });

        let Some(savepath) = savepath else {
            return Ok(cid.map(|value| value.to_string()));
        };

        let mut current = cid.unwrap_or("0").to_string();
        for segment in savepath
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
        {
            if segment == "." || segment == ".." {
                bail!("invalid savepath segment: {segment}");
            }
            current = if let Some(found) = self.find_child_directory(&current, segment).await? {
                found
            } else {
                self.mkdir(&current, segment).await?
            };
        }
        Ok(Some(current))
    }

    async fn find_child_directory(&self, parent_id: &str, name: &str) -> Result<Option<String>> {
        let mut offset = 0;
        loop {
            let value = self.list_dir(parent_id, offset, MAX_DIR_PAGE_LIMIT).await?;
            let total = value
                .get("count")
                .and_then(json_i64)
                .unwrap_or_default()
                .max(0);
            if let Some(items) = value.get("data").and_then(|value| value.as_array()) {
                for item in items {
                    if !entry_is_directory(item) {
                        continue;
                    }
                    if item_name(item).as_deref() == Some(name) {
                        return Ok(entry_id(item));
                    }
                }
            }
            offset += MAX_DIR_PAGE_LIMIT;
            if offset >= total {
                return Ok(None);
            }
        }
    }

    async fn list_dir(&self, parent_id: &str, offset: i64, limit: i64) -> Result<Value> {
        let value = self
            .request_json(
                self.req(Method::GET, API_FILE_LIST)?.query(&[
                    ("aid", "1".to_string()),
                    ("cid", parent_id.to_string()),
                    ("o", "file_name".to_string()),
                    ("asc", "1".to_string()),
                    ("offset", offset.to_string()),
                    ("show_dir", "1".to_string()),
                    ("limit", limit.to_string()),
                    ("snap", "0".to_string()),
                    ("natsort", "0".to_string()),
                    ("record_open_time", "1".to_string()),
                    ("format", "json".to_string()),
                    ("fc_mix", "0".to_string()),
                ]),
                API_FILE_LIST,
            )
            .await?;
        ensure_success(&value)?;
        Ok(value)
    }

    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String> {
        let value = self
            .request_json(
                self.req(Method::POST, API_DIR_ADD)?
                    .form(&[("pid", parent_id.to_string()), ("cname", name.to_string())]),
                API_DIR_ADD,
            )
            .await?;
        match ensure_success(&value) {
            Ok(()) => value
                .get("cid")
                .and_then(json_string)
                .context("missing cid in mkdir response"),
            Err(err) => {
                if let Some(api_err) = err.downcast_ref::<Pan115Error>() {
                    if api_err.kind() == Pan115ErrorKind::Exist {
                        if let Some(found) = self.find_child_directory(parent_id, name).await? {
                            return Ok(found);
                        }
                    }
                }
                Err(err)
            }
        }
    }
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

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn item_name(value: &Value) -> Option<String> {
    value.get("n").and_then(json_string)
}

fn entry_id(value: &Value) -> Option<String> {
    value
        .get("cid")
        .and_then(json_string)
        .or_else(|| value.get("fid").and_then(json_string))
}

fn entry_is_directory(value: &Value) -> bool {
    value
        .get("fid")
        .and_then(json_string)
        .map(|fid| fid.is_empty())
        .unwrap_or(true)
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
}
