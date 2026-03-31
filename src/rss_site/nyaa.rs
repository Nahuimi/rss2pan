use super::MagnetSite;

pub struct Nyaa;

impl MagnetSite for Nyaa {
    fn get_magnet(&self, item: &rss::Item) -> Option<String> {
        let hash = item
            .extensions()
            .get("nyaa")?
            .get("infoHash")?
            .first()?
            .value()?;
        Some(format!("magnet:?xt=urn:btih:{}", hash))
    }
}
