use rss::Item;

use super::{get_magnet_by_enclosure, MagnetSite};

pub struct Rsshub;

impl MagnetSite for Rsshub {
    fn get_magnet(&self, item: &Item) -> Option<String> {
        let magnet = get_magnet_by_enclosure(item);
        if magnet.is_empty() {
            None
        } else {
            Some(magnet)
        }
    }
}
