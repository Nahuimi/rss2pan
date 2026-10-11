use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Result};
use rusqlite::{params, params_from_iter, Connection, Error};

use crate::{
    rss_config::normalize_rss_url,
    rss_site::MagnetItem,
    utils::{
        canonicalize_magnet, current_db_timestamp, db_timestamp_months_ago, normalize_db_timestamp,
    },
};

const EXISTING_MAGNETS_CHUNK_SIZE: usize = 500;
const RSS_ITEMS_MAGNET_INDEX: &str = "idx_rss_items_magnet";
const RSS_BLACKLIST_LINK_INDEX: &str = "idx_rss_blacklist_rss_link";
const DEFAULT_LEGACY_BLACKLIST_FILE: &str = "blacklist.sqlite";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const RSS_ITEMS_TABLE: &str = "CREATE TABLE if not exists `rss_items` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `guid` VARCHAR(255), `pubDate` DATETIME, `creator` VARCHAR(255), `summary` TEXT, `content` VARCHAR(255), `isoDate` DATETIME, `categories` VARCHAR(255), `contentSnippet` VARCHAR(255), `done` TINYINT(1) DEFAULT 0, `magnet` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL, `cid` TEXT, `savepath` TEXT)";
const SITES_STATUS_TABLE: &str = "CREATE TABLE if not exists `sites_status` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `name` VARCHAR(255), `needLogin` TINYINT(1), `abnormalOp` TINYINT(1), `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)";
const RSS_BLACKLIST_TABLE: &str = "CREATE TABLE if not exists `rss_blacklist` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `rssLink` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)";

const RSS_ITEMS_OPTIONAL_COLUMNS: [(&str, &str); 2] = [("cid", "TEXT"), ("savepath", "TEXT")];

/// `rss_items` 里保存的单条连接以及它离线时使用的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRssItem {
    pub link: String,
    pub title: String,
    pub cid: Option<String>,
    pub savepath: Option<String>,
}

/// 旧的 `blacklist.sqlite` 合并进 `db.sqlite` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyBlacklistMerge {
    pub legacy_path: PathBuf,
    pub backup_path: PathBuf,
    pub imported: usize,
}

/// `db.sqlite` 里的全部数据：RSS 记录、站点状态和 RSS 黑名单。
///
/// 黑名单表和 RSS 表共用一个数据库文件，不再有单独的 `blacklist.sqlite`。
pub struct Database {
    pub rss: RssService,
    pub blacklist: BlacklistService,
}

pub struct RssService {
    conn: Connection,
}

pub struct BlacklistService {
    conn: Connection,
}

/// 打开 `db.sqlite`，并把旧的 `blacklist.sqlite` 合并进来。
pub fn open_database(
    db_path: impl AsRef<Path>,
    legacy_blacklist: Option<&Path>,
    retention_months: u32,
) -> Result<Database> {
    let db_path = db_path.as_ref();
    let rss = RssService::open_path(db_path)?;
    for merge in merge_legacy_blacklists(db_path, legacy_blacklist)? {
        log::info!(
            "merged {} blacklist rows from {} into {} (backup: {})",
            merge.imported,
            merge.legacy_path.display(),
            db_path.display(),
            merge.backup_path.display()
        );
    }
    let blacklist = BlacklistService::open_path(db_path, retention_months)?;
    Ok(Database { rss, blacklist })
}

/// 把旧 `blacklist.sqlite` 里的黑名单导入 `db.sqlite`，然后把旧文件改名为 `.bak`。
///
/// 候选文件是 `[paths].blacklist` 指定的路径和当前目录下的 `blacklist.sqlite`。
/// 读取失败时只记录警告，不会删除旧文件。
pub fn merge_legacy_blacklists(
    db_path: &Path,
    legacy_blacklist: Option<&Path>,
) -> Result<Vec<LegacyBlacklistMerge>> {
    let mut merges = Vec::new();
    for legacy_path in legacy_blacklist_candidates(legacy_blacklist) {
        if is_same_file(db_path, &legacy_path) || !legacy_path.is_file() {
            continue;
        }
        match merge_legacy_blacklist(db_path, &legacy_path) {
            Ok(merge) => merges.push(merge),
            Err(err) => log::warn!(
                "skip merging legacy blacklist {}: {err}",
                legacy_path.display()
            ),
        }
    }
    Ok(merges)
}

fn legacy_blacklist_candidates(legacy_blacklist: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut push = |path: PathBuf| {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    };
    if let Some(path) = legacy_blacklist.filter(|path| !path.as_os_str().is_empty()) {
        push(path.to_path_buf());
    }
    push(PathBuf::from(DEFAULT_LEGACY_BLACKLIST_FILE));
    candidates
}

fn is_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn merge_legacy_blacklist(db_path: &Path, legacy_path: &Path) -> Result<LegacyBlacklistMerge> {
    let legacy =
        Connection::open_with_flags(legacy_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let rows = {
        let mut stmt = legacy.prepare(
            "SELECT link, title, rssLink, createdAt, updatedAt FROM rss_blacklist ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    drop(legacy);

    let conn = open_connection(db_path)?;
    ensure_schema(&conn)?;
    let mut seen = existing_blacklist_urls(&conn)?;
    let now = current_db_timestamp();

    let mut imported = 0usize;
    let tx = conn.unchecked_transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO rss_blacklist (`link`,`title`,`rssLink`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5)",
        )?;
        for (link, title, rss_link, created_at, updated_at) in rows {
            let rss_link = normalize_rss_url(&rss_link);
            if rss_link.is_empty() || !seen.insert(rss_link.clone()) {
                continue;
            }
            insert.execute(params![
                link.unwrap_or_default(),
                title.unwrap_or_default(),
                rss_link,
                created_at
                    .and_then(|value| normalize_db_timestamp(&value))
                    .unwrap_or_else(|| now.clone()),
                updated_at
                    .and_then(|value| normalize_db_timestamp(&value))
                    .unwrap_or_else(|| now.clone()),
            ])?;
            imported += 1;
        }
    }
    tx.commit()?;
    drop(conn);

    let backup_path = legacy_backup_path(legacy_path);
    std::fs::rename(legacy_path, &backup_path)?;
    Ok(LegacyBlacklistMerge {
        legacy_path: legacy_path.to_path_buf(),
        backup_path,
        imported,
    })
}

fn existing_blacklist_urls(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT rssLink FROM rss_blacklist")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut urls = HashSet::new();
    for row in rows {
        urls.insert(normalize_rss_url(&row?));
    }
    Ok(urls)
}

fn legacy_backup_path(legacy_path: &Path) -> PathBuf {
    let backup = PathBuf::from(format!("{}.bak", legacy_path.display()));
    if !backup.exists() {
        return backup;
    }
    PathBuf::from(format!(
        "{}.bak-{}",
        legacy_path.display(),
        current_db_timestamp().replace([' ', '：', ':'], "")
    ))
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    Ok(conn)
}

#[cfg(test)]
fn open_memory_connection() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    Ok(conn)
}

/// 所有表都建在同一个数据库文件里，两个 service 使用同一份建表语句。
fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute(RSS_ITEMS_TABLE, ())?;
    conn.execute(SITES_STATUS_TABLE, ())?;
    conn.execute(RSS_BLACKLIST_TABLE, ())?;
    add_missing_rss_items_columns(conn)?;
    Ok(())
}

fn add_missing_rss_items_columns(conn: &Connection) -> Result<()> {
    let columns = table_columns(conn, "rss_items")?;
    for (name, column_type) in RSS_ITEMS_OPTIONAL_COLUMNS {
        if columns.iter().any(|column| column == name) {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE rss_items ADD COLUMN {name} {column_type}"),
            [],
        )?;
        log::info!("rss_items: added column {name}");
    }
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

impl RssService {
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(open_connection(path.as_ref())?)
    }

    #[cfg(test)]
    pub fn new_in_memory() -> Result<Self> {
        Self::from_connection(open_memory_connection()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        ensure_schema(&conn)?;
        let mut service = Self { conn };
        service.migrate_rss_items()?;
        service.normalize_table_timestamps("rss_items")?;
        service.normalize_table_timestamps("sites_status")?;
        Ok(service)
    }

    pub fn save_items(
        &mut self,
        items: &[MagnetItem],
        done: bool,
        cid: Option<&str>,
        savepath: Option<&str>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let now = current_db_timestamp();
        let done = u8::from(done).to_string();
        let cid = normalize_optional_value(cid);
        let savepath = normalize_optional_value(savepath);
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO rss_items (`link`,`title`,`content`,`magnet`,`done`,`createdAt`,`updatedAt`,`cid`,`savepath`) VALUES (?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(`magnet`) DO UPDATE SET
                    `cid` = COALESCE(rss_items.`cid`, excluded.`cid`),
                    `savepath` = COALESCE(rss_items.`savepath`, excluded.`savepath`)",
            )?;
            for item in items {
                let magnet = canonicalize_magnet(&item.magnet);
                if magnet.is_empty() {
                    continue;
                }
                stmt.execute(params![
                    item.link.as_str(),
                    item.title.as_str(),
                    item.content.as_deref().unwrap_or(""),
                    magnet.as_str(),
                    done.as_str(),
                    now.as_str(),
                    now.as_str(),
                    cid,
                    savepath,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn has_item(&self, magnet: &str) -> Result<bool> {
        let magnet = canonicalize_magnet(magnet);
        if magnet.is_empty() {
            return Ok(false);
        }
        let exists: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rss_items WHERE magnet = ?1)",
            [magnet.as_str()],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub fn existing_magnets(&self, magnets: &[String]) -> Result<HashSet<String>> {
        let mut normalized = magnets
            .iter()
            .map(|magnet| canonicalize_magnet(magnet))
            .filter(|magnet| !magnet.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_unstable();
        normalized.dedup();

        let mut existing = HashSet::new();
        for chunk in normalized.chunks(EXISTING_MAGNETS_CHUNK_SIZE) {
            let sql = format!(
                "SELECT magnet FROM rss_items WHERE magnet IN ({})",
                repeat_vars(chunk.len())
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params_from_iter(chunk.iter().map(|magnet| magnet.as_str())),
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                existing.insert(row?);
            }
        }
        Ok(existing)
    }

    /// 按 magnet 查出 `rss_items` 里保存的连接以及离线时用到的 `cid` / `savepath`。
    pub fn find_items_by_magnets(
        &self,
        magnets: &[String],
    ) -> Result<HashMap<String, StoredRssItem>> {
        let mut normalized = magnets
            .iter()
            .map(|magnet| canonicalize_magnet(magnet))
            .filter(|magnet| !magnet.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_unstable();
        normalized.dedup();

        let mut items = HashMap::new();
        for chunk in normalized.chunks(EXISTING_MAGNETS_CHUNK_SIZE) {
            let sql = format!(
                "SELECT magnet, link, title, cid, savepath FROM rss_items WHERE magnet IN ({})",
                repeat_vars(chunk.len())
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params_from_iter(chunk.iter().map(|magnet| magnet.as_str())),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        StoredRssItem {
                            link: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            cid: normalize_stored_value(row.get::<_, Option<String>>(3)?),
                            savepath: normalize_stored_value(row.get::<_, Option<String>>(4)?),
                        },
                    ))
                },
            )?;
            for row in rows {
                let (magnet, item) = row?;
                items.insert(magnet, item);
            }
        }
        Ok(items)
    }

    #[allow(dead_code)]
    pub fn get_item_by_magnet(&self, magnet: &str) -> Result<MagnetItem> {
        let magnet = canonicalize_magnet(magnet);
        let item = self.conn.query_row(
            "SELECT link,title,magnet FROM rss_items WHERE magnet = ?1",
            [magnet.as_str()],
            |row| {
                Ok(MagnetItem {
                    link: row.get(0)?,
                    title: row.get(1)?,
                    magnet: row.get(2)?,
                    content: None,
                    description: None,
                })
            },
        )?;
        Ok(item)
    }

    fn migrate_rss_items(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare("SELECT id, magnet FROM rss_items ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut keep_by_magnet = HashMap::new();
        let mut deletions = Vec::new();
        let mut updates = Vec::new();
        for (id, magnet) in rows {
            let canonical = canonicalize_magnet(&magnet);
            if keep_by_magnet.insert(canonical.clone(), id).is_some() {
                deletions.push(id);
                continue;
            }
            if magnet != canonical {
                updates.push((canonical, id));
            }
        }

        for id in deletions {
            tx.execute("DELETE FROM rss_items WHERE id = ?1", [id])?;
        }
        for (canonical, id) in updates {
            tx.execute(
                "UPDATE rss_items SET magnet = ?1 WHERE id = ?2",
                params![canonical, id],
            )?;
        }
        tx.execute(
            &format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {RSS_ITEMS_MAGNET_INDEX} ON rss_items(magnet)"
            ),
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    // @TODO 网站状态
    #[allow(dead_code)]
    pub fn update_status(&self, host: &str, key: &str, value: bool) -> Result<usize> {
        let column = match key {
            "needLogin" => "needLogin",
            "abnormalOp" => "abnormalOp",
            _ => bail!("invalid status column: {key}"),
        };
        let stmt = format!("UPDATE sites_status SET {column} = ?1, updatedAt = ?3 WHERE name = ?2");
        let num = self.conn.execute(
            &stmt,
            params![u8::from(value), host, current_db_timestamp()],
        )?;
        Ok(num)
    }

    #[allow(dead_code)]
    pub fn reset_status(&self, name: &str) -> Result<usize> {
        let num = self.conn.execute(
            "UPDATE sites_status SET abnormalOp = 0,needLogin = 0,updatedAt = ?2 WHERE name = ?1",
            params![name, current_db_timestamp()],
        )?;
        Ok(num)
    }

    #[allow(dead_code)]
    pub fn is_ready(&self, name: &str) -> Result<bool> {
        let r = self.conn.query_row(
            "SELECT needLogin,abnormalOp FROM sites_status WHERE name = ?1",
            [name],
            |row| <(u8, u8)>::try_from(row),
        );
        match r {
            Ok((0, 0)) => Ok(true),
            Ok(_) => Ok(false),
            Err(Error::QueryReturnedNoRows) => {
                let now = current_db_timestamp();
                self.conn.execute(
                    "INSERT INTO sites_status (name,`createdAt`,`updatedAt`,`needLogin`, `abnormalOp`) VALUES (?,?,?,0,0)",
                    [name, now.as_str(), now.as_str()],
                )?;
                Ok(true)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn normalize_table_timestamps(&mut self, table: &str) -> Result<()> {
        normalize_table_timestamps(&mut self.conn, table)
    }
}

impl BlacklistService {
    pub fn open_path(path: impl AsRef<Path>, retention_months: u32) -> Result<Self> {
        let mut service = Self::from_connection(open_connection(path.as_ref())?)?;
        service.prune_expired(retention_months)?;
        Ok(service)
    }

    #[cfg(test)]
    pub fn new_in_memory(retention_months: u32) -> Result<Self> {
        let mut service = Self::from_connection(open_memory_connection()?)?;
        service.prune_expired(retention_months)?;
        Ok(service)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        ensure_schema(&conn)?;
        let mut service = Self { conn };
        service.migrate_blacklist_schema()?;
        service.migrate_blacklist_items()?;
        service.normalize_table_timestamps()?;
        Ok(service)
    }

    pub fn prune_expired(&mut self, retention_months: u32) -> Result<usize> {
        let rows = self.conn.execute(
            "DELETE FROM rss_blacklist WHERE updatedAt < ?1",
            params![db_timestamp_months_ago(retention_months)],
        )?;
        Ok(rows)
    }

    pub fn contains_rss_url(&self, rss_url: &str) -> Result<bool> {
        let rss_link = normalize_rss_url(rss_url);
        let exists: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rss_blacklist WHERE rssLink = ?1)",
            [rss_link.as_str()],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub fn blacklist_rss(&self, rss_url: &str, item: &MagnetItem) -> Result<()> {
        let now = current_db_timestamp();
        let rss_link = normalize_rss_url(rss_url);
        self.conn.execute(
            "INSERT INTO rss_blacklist (`link`,`title`,`rssLink`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?4)
             ON CONFLICT(`rssLink`) DO UPDATE SET `link` = excluded.`link`, `title` = excluded.`title`, `updatedAt` = excluded.`updatedAt`",
            params![item.link.as_str(), item.title.as_str(), rss_link, now],
        )?;
        Ok(())
    }

    fn migrate_blacklist_schema(&mut self) -> Result<()> {
        let has_rss_key = table_columns(&self.conn, "rss_blacklist")?
            .iter()
            .any(|column| column == "rssKey");

        if !has_rss_key {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        tx.execute("ALTER TABLE rss_blacklist RENAME TO rss_blacklist_old", [])?;
        tx.execute(RSS_BLACKLIST_TABLE, [])?;
        tx.execute(
            "INSERT INTO rss_blacklist (`id`,`link`,`title`,`rssLink`,`createdAt`,`updatedAt`)
             SELECT `id`,`link`,`title`,`rssLink`,`createdAt`,`updatedAt` FROM rss_blacklist_old",
            [],
        )?;
        tx.execute("DROP TABLE rss_blacklist_old", [])?;
        tx.commit()?;
        Ok(())
    }

    fn migrate_blacklist_items(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare("SELECT id, rssLink FROM rss_blacklist ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut keep_by_rss_link = HashMap::new();
        let mut deletions = Vec::new();
        let mut updates = Vec::new();
        for (id, rss_link) in rows {
            let normalized = normalize_rss_url(&rss_link);
            if keep_by_rss_link.insert(normalized.clone(), id).is_some() {
                deletions.push(id);
                continue;
            }
            if rss_link != normalized {
                updates.push((normalized, id));
            }
        }

        for id in deletions {
            tx.execute("DELETE FROM rss_blacklist WHERE id = ?1", [id])?;
        }
        for (rss_link, id) in updates {
            tx.execute(
                "UPDATE rss_blacklist SET rssLink = ?1 WHERE id = ?2",
                params![rss_link, id],
            )?;
        }
        tx.execute("DROP INDEX IF EXISTS idx_rss_blacklist_rss_key", [])?;
        tx.execute(
            &format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {RSS_BLACKLIST_LINK_INDEX} ON rss_blacklist(rssLink)"
            ),
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn normalize_table_timestamps(&mut self) -> Result<()> {
        normalize_table_timestamps(&mut self.conn, "rss_blacklist")
    }
}

fn normalize_table_timestamps(conn: &mut Connection, table: &str) -> Result<()> {
    let tx = conn.transaction()?;
    let rows = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id, createdAt, updatedAt FROM {table} ORDER BY id"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for (id, created_at, updated_at) in rows {
        let normalized_created =
            normalize_db_timestamp(&created_at).unwrap_or_else(|| created_at.clone());
        let normalized_updated =
            normalize_db_timestamp(&updated_at).unwrap_or_else(|| updated_at.clone());
        if normalized_created == created_at && normalized_updated == updated_at {
            continue;
        }
        tx.execute(
            &format!("UPDATE {table} SET createdAt = ?1, updatedAt = ?2 WHERE id = ?3"),
            params![normalized_created, normalized_updated, id],
        )?;
    }

    tx.commit()?;
    Ok(())
}

fn repeat_vars(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_optional_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalize_stored_value(value: Option<String>) -> Option<String> {
    normalize_optional_value(value.as_deref()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn magnet(hash: &str) -> String {
        format!("magnet:?xt=urn:btih:{hash}")
    }

    fn magnet_item(magnet: &str) -> MagnetItem {
        MagnetItem {
            title: "title".to_string(),
            link: "link".to_string(),
            magnet: magnet.to_string(),
            content: None,
            description: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn get_item_test() {
        let service = RssService::new_in_memory().unwrap();
        let r = service.get_item_by_magnet("magnet");
        assert!(r.is_err());
    }

    #[test]
    fn update_status_test() {
        let host = "115.com";
        let key = "abnormalOp";
        let value = false;
        let service = RssService::new_in_memory().unwrap();
        let r = service.update_status(host, key, value);
        println!("{:?}", r);
    }

    #[test]
    fn is_ready_test() {
        let host = "114.com";
        let service = RssService::new_in_memory().unwrap();
        let r = service.is_ready(host).unwrap();
        assert!(r);
    }

    #[test]
    fn test_has_item_matches_canonicalized_magnet() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(
                &[magnet_item("magnet:?dn=test&xt=URN:BTIH:ABCDEF")],
                true,
                None,
                None,
            )
            .unwrap();
        assert!(service.has_item("magnet:?xt=urn:btih:abcdef").unwrap());
    }

    #[test]
    fn test_existing_magnets_returns_matches() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(
                &[magnet_item(&magnet("abc")), magnet_item(&magnet("def"))],
                true,
                None,
                None,
            )
            .unwrap();
        let existing = service
            .existing_magnets(&["magnet:?xt=URN:BTIH:ABC&dn=test".to_string(), magnet("xyz")])
            .unwrap();
        assert_eq!(existing, HashSet::from([magnet("abc")]));
    }

    #[test]
    fn test_save_items_deduplicates_canonical_magnet() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(
                &[
                    magnet_item("magnet:?xt=urn:btih:abc123&dn=foo"),
                    magnet_item("magnet:?tr=udp://tracker&xt=URN:BTIH:ABC123"),
                ],
                true,
                None,
                None,
            )
            .unwrap();
        let count: i64 = service
            .conn
            .query_row("SELECT COUNT(*) FROM rss_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let stored = service
            .get_item_by_magnet("magnet:?xt=urn:btih:abc123")
            .unwrap();
        assert_eq!(stored.magnet, magnet("abc123"));
    }

    #[test]
    fn test_save_items_stores_cid_and_savepath() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(
                &[magnet_item(&magnet("abc"))],
                true,
                Some("2479224057885794455"),
                Some("番剧/测试"),
            )
            .unwrap();

        let stored = service.find_items_by_magnets(&[magnet("abc")]).unwrap();
        let item = stored.get(&magnet("abc")).unwrap();
        assert_eq!(item.cid.as_deref(), Some("2479224057885794455"));
        assert_eq!(item.savepath.as_deref(), Some("番剧/测试"));
        assert_eq!(item.link, "link");
        assert_eq!(item.title, "title");
    }

    #[test]
    fn test_save_items_keeps_first_cid_and_savepath() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(
                &[magnet_item(&magnet("abc"))],
                true,
                Some("111"),
                Some("old"),
            )
            .unwrap();
        service
            .save_items(
                &[magnet_item(&magnet("abc"))],
                true,
                Some("222"),
                Some("new"),
            )
            .unwrap();

        let stored = service.find_items_by_magnets(&[magnet("abc")]).unwrap();
        let item = stored.get(&magnet("abc")).unwrap();
        assert_eq!(item.cid.as_deref(), Some("111"));
        assert_eq!(item.savepath.as_deref(), Some("old"));
    }

    #[test]
    fn test_save_items_fills_missing_cid_and_savepath_later() {
        let mut service = RssService::new_in_memory().unwrap();
        service
            .save_items(&[magnet_item(&magnet("abc"))], true, None, None)
            .unwrap();
        service
            .save_items(
                &[magnet_item(&magnet("abc"))],
                true,
                Some("222"),
                Some("new"),
            )
            .unwrap();

        let stored = service.find_items_by_magnets(&[magnet("abc")]).unwrap();
        let item = stored.get(&magnet("abc")).unwrap();
        assert_eq!(item.cid.as_deref(), Some("222"));
        assert_eq!(item.savepath.as_deref(), Some("new"));
    }

    #[test]
    fn test_rss_service_adds_cid_and_savepath_to_legacy_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE rss_items (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `guid` VARCHAR(255), `pubDate` DATETIME, `creator` VARCHAR(255), `summary` TEXT, `content` VARCHAR(255), `isoDate` DATETIME, `categories` VARCHAR(255), `contentSnippet` VARCHAR(255), `done` TINYINT(1) DEFAULT 0, `magnet` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rss_items (`link`,`title`,`content`,`magnet`,`done`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                "https://example.com/item",
                "test",
                "",
                magnet("abc123"),
                "1",
                current_db_timestamp(),
                current_db_timestamp(),
            ],
        )
        .unwrap();

        let service = RssService::from_connection(conn).unwrap();
        let columns = table_columns(&service.conn, "rss_items").unwrap();
        assert!(columns.iter().any(|column| column == "cid"));
        assert!(columns.iter().any(|column| column == "savepath"));

        let stored = service.find_items_by_magnets(&[magnet("abc123")]).unwrap();
        assert_eq!(stored[&magnet("abc123")].cid, None);
    }

    #[test]
    fn test_open_database_keeps_rss_and_blacklist_in_one_file() {
        let dir = temp_dir("rss2pan-one-file");
        let db_path = dir.join("db.sqlite");

        let database = open_database(&db_path, None, 6).unwrap();
        let tables = {
            let mut stmt = database
                .rss
                .conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .unwrap();
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<HashSet<_>, _>>()
                .unwrap();
            rows
        };
        assert!(tables.contains("rss_items"));
        assert!(tables.contains("rss_blacklist"));
        assert!(!dir.join("blacklist.sqlite").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_database_merges_legacy_blacklist_file() {
        let dir = temp_dir("rss2pan-merge");
        let db_path = dir.join("db.sqlite");
        let legacy_path = dir.join("blacklist.sqlite");
        let now = current_db_timestamp();

        {
            let conn = Connection::open(&legacy_path).unwrap();
            conn.execute(RSS_BLACKLIST_TABLE, []).unwrap();
            conn.execute(
                "INSERT INTO rss_blacklist (`link`,`title`,`rssLink`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5)",
                params![
                    "https://example.com/item",
                    "合集",
                    "https://example.com/rss?b=2&a=1",
                    now.as_str(),
                    now.as_str(),
                ],
            )
            .unwrap();
        }

        let database = open_database(&db_path, Some(&legacy_path), 6).unwrap();
        assert!(database
            .blacklist
            .contains_rss_url("https://example.com/rss?a=1&b=2")
            .unwrap());
        assert!(!legacy_path.exists());
        assert!(dir.join("blacklist.sqlite.bak").exists());
        drop(database);

        // 第二次打开时旧文件已经改名为 .bak，不会重复导入
        let database = open_database(&db_path, Some(&legacy_path), 6).unwrap();
        let count: i64 = database
            .blacklist
            .conn
            .query_row("SELECT COUNT(*) FROM rss_blacklist", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_database_skips_legacy_file_that_is_not_sqlite() {
        let dir = temp_dir("rss2pan-broken-legacy");
        let db_path = dir.join("db.sqlite");
        let legacy_path = dir.join("blacklist.sqlite");
        std::fs::write(&legacy_path, b"not a sqlite file").unwrap();

        let database = open_database(&db_path, Some(&legacy_path), 6).unwrap();
        let count: i64 = database
            .blacklist
            .conn
            .query_row("SELECT COUNT(*) FROM rss_blacklist", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(legacy_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_open_path_initializes_database_file() {
        let path = std::env::temp_dir().join(format!(
            "rss2pan-db-{}-{}.sqlite",
            std::process::id(),
            rand::random::<u64>()
        ));

        let service = RssService::open_path(&path).unwrap();
        let count: i64 = service
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'rss_items'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_rss_service_normalizes_legacy_timestamp_formats() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE rss_items (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `guid` VARCHAR(255), `pubDate` DATETIME, `creator` VARCHAR(255), `summary` TEXT, `content` VARCHAR(255), `isoDate` DATETIME, `categories` VARCHAR(255), `contentSnippet` VARCHAR(255), `done` TINYINT(1) DEFAULT 0, `magnet` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE sites_status (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `name` VARCHAR(255), `needLogin` TINYINT(1), `abnormalOp` TINYINT(1), `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rss_items (`link`,`title`,`content`,`magnet`,`done`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                "https://example.com/item",
                "test",
                "",
                magnet("abc123"),
                "1",
                "2026-04-07T03:13:16.728787900+00:00",
                "2026-04-05 07：57：40.658769500 UTC",
            ],
        )
        .unwrap();

        let service = RssService::from_connection(conn).unwrap();
        let timestamps: (String, String) = service
            .conn
            .query_row(
                "SELECT createdAt, updatedAt FROM rss_items WHERE magnet = ?1",
                [magnet("abc123")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(timestamps.0, "2026-04-07 11：13");
        assert_eq!(timestamps.1, "2026-04-05 15：57");
    }

    #[test]
    fn test_blacklist_service_tracks_rss_urls_by_normalized_key() {
        let service = BlacklistService::new_in_memory(6).unwrap();
        let item = MagnetItem {
            title: "合集".to_string(),
            link: "https://example.com/item".to_string(),
            magnet: magnet("abc"),
            content: None,
            description: None,
        };

        service
            .blacklist_rss(
                "https://example.com/rss?subgroupid=12&bangumiId=2739",
                &item,
            )
            .unwrap();

        assert!(service
            .contains_rss_url("https://example.com/rss?bangumiId=2739&subgroupid=12")
            .unwrap());
    }

    #[test]
    fn test_blacklist_service_prunes_expired_rows() {
        let mut service = BlacklistService::new_in_memory(6).unwrap();
        service
            .conn
            .execute(
                "INSERT INTO rss_blacklist (`link`,`title`,`rssLink`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5)",
                params![
                    "https://example.com/item",
                    "old",
                    normalize_rss_url("https://example.com/rss"),
                    "2025-01-01T00:00:00Z",
                    "2025-01-01T00:00:00Z",
                ],
            )
            .unwrap();

        let deleted = service.prune_expired(6).unwrap();
        assert_eq!(deleted, 1);
        assert!(!service.contains_rss_url("https://example.com/rss").unwrap());
    }

    #[test]
    fn test_blacklist_service_migrates_old_rsskey_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE rss_blacklist (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `rssLink` TEXT NOT NULL, `rssKey` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rss_blacklist (`link`,`title`,`rssLink`,`rssKey`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                "https://example.com/item",
                "old",
                "https://example.com/rss?b=2&a=1",
                "example.com/rss?a=1&b=2",
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            ],
        )
        .unwrap();

        let service = BlacklistService::from_connection(conn).unwrap();

        assert!(service
            .contains_rss_url("https://example.com/rss?a=1&b=2")
            .unwrap());

        let columns = {
            let mut stmt = service
                .conn
                .prepare("PRAGMA table_info(rss_blacklist)")
                .unwrap();
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert!(!columns.iter().any(|column| column == "rssKey"));
    }
}
