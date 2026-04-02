use std::collections::{HashMap, HashSet};

use chrono::prelude::*;

use anyhow::{bail, Result};
use rusqlite::{params, params_from_iter, Connection, Error};

use crate::{rss_site::MagnetItem, utils::canonicalize_magnet};

const EXISTING_MAGNETS_CHUNK_SIZE: usize = 500;
const RSS_ITEMS_MAGNET_INDEX: &str = "idx_rss_items_magnet";

pub struct RssService {
    conn: Connection,
}

impl RssService {
    pub fn new() -> Result<Self> {
        Self::open("db.sqlite")
    }

    #[cfg(test)]
    pub fn new_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn open(path: &str) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
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
        Ok(service)
    }

    pub fn save_items(&mut self, items: &[MagnetItem], done: bool) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let now: DateTime<Utc> = Utc::now();
        let now = now.to_string();
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
        let stmt = format!("UPDATE sites_status SET {column} = ?1 WHERE name = ?2");
        let num = self.conn.execute(&stmt, params![u8::from(value), host])?;
        Ok(num)
    }

    #[allow(dead_code)]
    pub fn reset_status(&self, name: &str) -> Result<usize> {
        let num = self.conn.execute(
            "UPDATE sites_status SET abnormalOp = 0,needLogin = 0 WHERE name = ?1",
            [name],
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
                let now: DateTime<Utc> = Utc::now();
                self.conn.execute(
                    "INSERT INTO sites_status (name,`createdAt`,`updatedAt`,`needLogin`, `abnormalOp`) VALUES (?,?,?,0,0)",
                    [name, &now.to_string(), &now.to_string()],
                )?;
                Ok(true)
            }
            Err(err) => Err(err.into()),
        }
    }
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
    fn test_migration_normalizes_and_deduplicates_existing_rows() {
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
            params!["l1", "t1", "", "magnet:?dn=foo&xt=URN:BTIH:ABC", "1", "now", "now"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rss_items (`link`,`title`,`content`,`magnet`,`done`,`createdAt`,`updatedAt`) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params!["l2", "t2", "", "magnet:?xt=urn:btih:abc&tr=udp://tracker", "1", "now", "now"],
        )
        .unwrap();

        let service = RssService::from_connection(conn).unwrap();
        let count: i64 = service
            .conn
            .query_row("SELECT COUNT(*) FROM rss_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let stored = service
            .get_item_by_magnet("magnet:?xt=urn:btih:abc")
            .unwrap();
        assert_eq!(stored.magnet, magnet("abc"));
        let index_count: i64 = service
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_index_list('rss_items') WHERE name = ?1",
                [RSS_ITEMS_MAGNET_INDEX],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }
}
