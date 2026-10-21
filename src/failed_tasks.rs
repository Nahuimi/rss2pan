//! 提交失败时把失败的链接、`cid`、`savepath` 等写入 JSON 文件。
//!
//! 文件默认放在 `db.sqlite` 同目录，文件名是 [`FAILED_TASKS_FILE_NAME`]。每次提交失败
//! 都会把失败的批次追加进去，方便之后再手动重试。文件格式：
//!
//! ```json
//! {
//!   "updated_at": "2026-10-21T17:30:00+08:00",
//!   "failures": [
//!     {
//!       "created_at": "2026-10-21T17:30:00+08:00",
//!       "source": "test",
//!       "cid": "2479224057885794455",
//!       "savepath": "番剧/测试",
//!       "error": "unexpected 115 error (code 0): 单次最多添加115个链接",
//!       "links": ["magnet:?xt=urn:btih:..."]
//!     }
//!   ]
//! }
//! ```

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::utils::current_beijing_rfc3339;

/// 提交失败记录的文件名。
pub const FAILED_TASKS_FILE_NAME: &str = "failed_tasks.json";

/// 同一个进程里可能有多个请求同时写失败记录，串行化“读-改-写”。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 一个提交失败的批次。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedSubmit {
    pub created_at: String,
    /// 失败时正在处理的来源，比如 RSS 配置名、`[magnet]`、`[server]`、`[retry]`。
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savepath: Option<String>,
    pub error: String,
    pub links: Vec<String>,
}

impl FailedSubmit {
    pub fn new(
        source: impl Into<String>,
        cid: Option<String>,
        savepath: Option<String>,
        error: impl Into<String>,
        links: Vec<String>,
    ) -> Self {
        Self {
            created_at: current_beijing_rfc3339(),
            source: source.into(),
            cid,
            savepath,
            error: error.into(),
            links,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FailedTasksFile {
    pub updated_at: String,
    pub failures: Vec<FailedSubmit>,
}

impl FailedTasksFile {
    pub fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn link_count(&self) -> usize {
        self.failures
            .iter()
            .map(|failure| failure.links.len())
            .sum()
    }
}

/// `db.sqlite` 同目录下的 `failed_tasks.json`；数据库是相对路径时就是当前目录。
pub fn path_for_database(database: &Path) -> PathBuf {
    match database
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent.join(FAILED_TASKS_FILE_NAME),
        None => PathBuf::from(FAILED_TASKS_FILE_NAME),
    }
}

/// 读取失败记录。文件不存在或为空时返回空记录；内容是非法 JSON 时返回错误。
pub fn read(path: &Path) -> Result<FailedTasksFile> {
    if !path.exists() {
        return Ok(FailedTasksFile::default());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("read {} failed", path.display()))?;
    if content.trim().is_empty() {
        return Ok(FailedTasksFile::default());
    }
    serde_json::from_str(&content).with_context(|| format!("parse {} failed", path.display()))
}

/// 把失败的批次追加到 `path`。文件不存在时会创建，内容是非法 JSON 时先备份再重建。
pub fn append(path: &Path, failures: &[FailedSubmit]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }

    let _guard = WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let mut file = load_for_append(path)?;
    file.updated_at = current_beijing_rfc3339();
    file.failures.extend(failures.iter().cloned());
    write_locked(path, &file)
}

/// 重写整个失败记录文件，用于重试成功后删掉已经提交的链接。
pub fn write(path: &Path, file: &FailedTasksFile) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    write_locked(path, file)
}

fn load_for_append(path: &Path) -> Result<FailedTasksFile> {
    if !path.exists() {
        return Ok(FailedTasksFile::default());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("read {} failed", path.display()))?;
    if content.trim().is_empty() {
        return Ok(FailedTasksFile::default());
    }

    match serde_json::from_str(&content) {
        Ok(file) => Ok(file),
        Err(err) => {
            // 旧文件损坏时先备份，保证这次失败的链接不会丢。
            let backup = backup_path(path);
            fs::rename(path, &backup).with_context(|| {
                format!(
                    "backup invalid failed tasks file {} to {} failed",
                    path.display(),
                    backup.display()
                )
            })?;
            log::warn!(
                "failed tasks file {} is not valid JSON ({err}), moved to {}",
                path.display(),
                backup.display()
            );
            Ok(FailedTasksFile::default())
        }
    }
}

fn write_locked(path: &Path, file: &FailedTasksFile) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {} failed", parent.display()))?;
    }

    let mut content = serde_json::to_string_pretty(file)
        .with_context(|| format!("serialize {} failed", path.display()))?;
    content.push('\n');
    fs::write(path, content).with_context(|| format!("write {} failed", path.display()))
}

fn backup_path(path: &Path) -> PathBuf {
    let suffix = current_beijing_rfc3339().replace([':', '+'], "-");
    path.with_extension(format!("json.bak-{suffix}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let seq = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rss2pan-failed-tasks-{}-{name}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(link: &str) -> FailedSubmit {
        FailedSubmit::new(
            "test",
            Some("123".to_string()),
            Some("番剧/测试".to_string()),
            "unexpected 115 error",
            vec![link.to_string()],
        )
    }

    fn read_file(path: &Path) -> FailedTasksFile {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn test_path_for_database_relative_and_absolute() {
        assert_eq!(
            path_for_database(Path::new("db.sqlite")),
            PathBuf::from(FAILED_TASKS_FILE_NAME)
        );
        assert_eq!(
            path_for_database(Path::new("data/db.sqlite")),
            PathBuf::from("data").join(FAILED_TASKS_FILE_NAME)
        );
        assert_eq!(
            path_for_database(Path::new("/tmp/rss2pan/db.sqlite")),
            PathBuf::from("/tmp/rss2pan").join(FAILED_TASKS_FILE_NAME)
        );
    }

    #[test]
    fn test_append_creates_file_and_accumulates_failures() {
        let dir = temp_dir("append");
        let path = dir.join(FAILED_TASKS_FILE_NAME);

        append(&path, &[sample("magnet:?xt=urn:btih:aaa")]).unwrap();
        let file = read_file(&path);
        assert_eq!(file.failures.len(), 1);
        assert_eq!(file.failures[0].source, "test");
        assert_eq!(file.failures[0].cid.as_deref(), Some("123"));
        assert_eq!(file.failures[0].savepath.as_deref(), Some("番剧/测试"));
        assert_eq!(file.failures[0].links, vec!["magnet:?xt=urn:btih:aaa"]);
        assert!(!file.updated_at.is_empty());

        append(
            &path,
            &[
                sample("magnet:?xt=urn:btih:bbb"),
                sample("magnet:?xt=urn:btih:ccc"),
            ],
        )
        .unwrap();
        assert_eq!(read_file(&path).failures.len(), 3);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_append_does_nothing_for_empty_failures() {
        let dir = temp_dir("empty");
        let path = dir.join(FAILED_TASKS_FILE_NAME);

        append(&path, &[]).unwrap();
        assert!(!path.exists());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_append_recovers_from_invalid_json() {
        let dir = temp_dir("invalid");
        let path = dir.join(FAILED_TASKS_FILE_NAME);
        fs::write(&path, "not json").unwrap();

        append(&path, &[sample("magnet:?xt=urn:btih:aaa")]).unwrap();

        assert_eq!(read_file(&path).failures.len(), 1);
        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups, 1);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_read_missing_file_returns_empty_and_invalid_json_errors() {
        let dir = temp_dir("read");
        let path = dir.join(FAILED_TASKS_FILE_NAME);

        let file = read(&path).unwrap();
        assert!(file.is_empty());
        assert_eq!(file.link_count(), 0);

        fs::write(&path, "not json").unwrap();
        assert!(read(&path).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_write_rewrites_and_prunes_retried_links() {
        let dir = temp_dir("write");
        let path = dir.join(FAILED_TASKS_FILE_NAME);

        append(
            &path,
            &[
                sample("magnet:?xt=urn:btih:aaa"),
                sample("magnet:?xt=urn:btih:bbb"),
            ],
        )
        .unwrap();

        let mut file = read(&path).unwrap();
        assert_eq!(file.link_count(), 2);
        file.failures.truncate(1);
        write(&path, &file).unwrap();

        let file = read(&path).unwrap();
        assert_eq!(file.link_count(), 1);
        assert_eq!(file.failures[0].links, vec!["magnet:?xt=urn:btih:aaa"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_failed_submit_is_json_serializable_without_optional_fields() {
        let entry = FailedSubmit::new("magnet", None, None, "error", vec!["link".to_string()]);
        let value = serde_json::to_value(&entry).unwrap();
        assert!(value.get("cid").is_none());
        assert!(value.get("savepath").is_none());
        assert_eq!(value["source"], "magnet");
    }
}
