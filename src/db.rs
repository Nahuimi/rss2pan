use std::{
    collections::{HashMap, HashSet},
    path::Path,
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

pub struct RssService {
    conn: Connection,
}

pub struct BlacklistService {
    conn: Connection,
}

impl RssService {
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path.as_ref())?)
    }

    #[cfg(test)]
    pub fn new_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute(
            "CREATE TABLE if not exists `rss_items` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `guid` VARCHAR(255), `pubDate` DATETIME, `creator` VARCHAR(255), `summary` TEXT, `content` VARCHAR(255), `isoDate` DATETIME, `categories` VARCHAR(255), `contentSnippet` VARCHAR(255), `done` TINYINT(1) DEFAULT 0, `magnet` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE if not exists `sites_status` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `name` VARCHAR(255), `needLogin` TINYINT(1), `abnormalOp` TINYINT(1), `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            (),
        )?;
        let mut service = Self { conn };
        service.migrate_rss_items()?;
        service.normalize_table_timestamps("rss_items")?;
        service.normalize_table_timestamps("sites_status")?;
        Ok(service)
    }

    pub fn save_items(&mut self, items: &[MagnetItem], done: bool) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let now = current_db_timestamp();
        let done = u8::from(done).to_string();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO rss_items (`link`,`title`,`content`,`magnet`,`done`,`createdAt`,`updatedAt`) VALUES (?,?,?,?,?,?,?)",
            )?;
            for item in items {
                let magnet = canonicalize_magnet(&item.magnet);
                if magnet.is_empty() {
                    continue;
                }
                stmt.execute([
                    item.link.as_str(),
                    item.title.as_str(),
                    item.content.as_deref().unwrap_or(""),
                    magnet.as_str(),
                    done.as_str(),
                    now.as_str(),
                    now.as_str(),
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
        let mut service = Self::from_connection(Connection::open(path.as_ref())?)?;
        service.prune_expired(retention_months)?;
        Ok(service)
    }

    #[cfg(test)]
    pub fn new_in_memory(retention_months: u32) -> Result<Self> {
        let mut service = Self::from_connection(Connection::open_in_memory()?)?;
        service.prune_expired(retention_months)?;
        Ok(service)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute(
            "CREATE TABLE if not exists `rss_blacklist` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `rssLink` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            (),
        )?;
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
        let has_rss_key = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(rss_blacklist)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .any(|column| column == "rssKey")
        };

        if !has_rss_key {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        tx.execute("ALTER TABLE rss_blacklist RENAME TO rss_blacklist_old", [])?;
        tx.execute(
            "CREATE TABLE rss_blacklist (`id` INTEGER PRIMARY KEY AUTOINCREMENT, `link` VARCHAR(255), `title` VARCHAR(255), `rssLink` TEXT NOT NULL, `createdAt` DATETIME NOT NULL, `updatedAt` DATETIME NOT NULL)",
            [],
        )?;
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
            .save_items(&[magnet_item("magnet:?dn=test&xt=URN:BTIH:ABCDEF")], true)
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
