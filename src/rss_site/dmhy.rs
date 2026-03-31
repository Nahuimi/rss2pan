use super::MagnetSite;

pub struct Dmhy;

impl MagnetSite for Dmhy {
    fn get_magnet(&self, item: &rss::Item) -> Option<String> {
        let url = &item.enclosure()?.url;
        let idx = url.find("&dn=");
        if let Some(idx) = idx {
            Some(url[..idx].to_string())
        } else {
            Some(url.to_string())
        }
    }
}
