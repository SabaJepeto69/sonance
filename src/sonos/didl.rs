//! DIDL-Lite: the metadata format Sonos uses for tracks, queues and favorites.

use super::xml::{attr, elements, escape, tag};

#[derive(Clone, Debug, Default)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: Option<String>,
    pub uri: String,
    /// Metadata to send back when playing this item (a favorite's `r:resMD`).
    pub meta: String,
    pub class: String,
    pub description: String,
    pub duration: u32,
}

impl Item {
    pub fn is_container(&self) -> bool {
        self.uri.starts_with("x-rincon-cpcontainer:")
            || self.meta.contains("object.container")
            || self.uri.starts_with("file:///jffs/settings/savedqueues")
    }
}

const NS: &str = r#"xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/""#;

/// "H:MM:SS" -> seconds; anything unparsable (NOT_IMPLEMENTED, "") is 0.
pub fn parse_time(s: &str) -> u32 {
    let mut total = 0u32;
    for part in s.split(':') {
        let Ok(n) = part.split('.').next().unwrap_or("").parse::<u32>() else { return 0 };
        total = total * 60 + n;
    }
    total
}

pub fn fmt_time(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

pub fn hms(secs: u32) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Album art URIs are often relative to the speaker that served them.
pub fn absolute_art(art: Option<String>, ip: &str) -> Option<String> {
    let art = art.filter(|a| !a.is_empty())?;
    Some(if art.starts_with('/') { format!("http://{ip}:1400{art}") } else { art })
}

/// Parses an (already unescaped) DIDL-Lite document into items and containers.
pub fn parse(didl: &str, ip: &str) -> Vec<Item> {
    let mut els = elements(didl, "item");
    els.extend(elements(didl, "container"));
    els.into_iter()
        .map(|el| {
            let res = elements(el, "res").first().copied().unwrap_or("");
            Item {
                id: attr(el, "id").unwrap_or_default(),
                title: tag(el, "dc:title").unwrap_or_default(),
                artist: tag(el, "dc:creator").unwrap_or_default(),
                album: tag(el, "upnp:album").unwrap_or_default(),
                art: absolute_art(tag(el, "upnp:albumArtURI"), ip),
                uri: tag(el, "res").unwrap_or_default(),
                meta: tag(el, "r:resMD").unwrap_or_default(),
                class: tag(el, "upnp:class").unwrap_or_default(),
                description: tag(el, "r:description").unwrap_or_default(),
                duration: attr(res, "duration").map(|d| parse_time(&d)).unwrap_or(0),
            }
        })
        .collect()
}

/// Minimal metadata Sonos accepts for a music-service item.
pub fn service_meta(id: &str, title: &str, class: &str, desc: &str) -> String {
    format!(
        r#"<DIDL-Lite {NS}><item id="{}" parentID="-1" restricted="true"><dc:title>{}</dc:title><upnp:class>{}</upnp:class><desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/">{}</desc></item></DIDL-Lite>"#,
        escape(id),
        escape(title),
        class,
        escape(desc)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("0:03:21"), 201);
        assert_eq!(parse_time("1:00:00.000"), 3600);
        assert_eq!(parse_time("NOT_IMPLEMENTED"), 0);
        assert_eq!(fmt_time(201), "3:21");
        assert_eq!(hms(3725), "01:02:05");
    }
}
