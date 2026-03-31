use rss::Item;

use super::MagnetSite;

pub struct Mikanani;

impl MagnetSite for Mikanani {
    fn get_magnet(&self, item: &Item) -> Option<String> {
        let link = item.link()?;
        let idx = link.find("Episode/")?;
        Some(format!("magnet:?xt=urn:btih:{}", &link[idx + 8..]))
    }
}
