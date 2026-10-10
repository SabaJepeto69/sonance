//! Synced lyrics from LRCLIB (lrclib.net): free, open and needs no account.

use anyhow::Result;
use serde_json::Value;

const API: &str = "https://lrclib.net/api";
const AGENT: &str = concat!("Sonance/", env!("CARGO_PKG_VERSION"), " (https://github.com/SabaJepeto69/sonance)");

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lyrics {
    /// (seconds, line), sorted. Empty lines are kept: they're the pauses.
    pub lines: Vec<(f32, String)>,
}

impl Lyrics {
    /// Index of the line being sung at `pos` seconds, if one has started.
    pub fn index_at(&self, pos: f32) -> Option<usize> {
        let n = self.lines.partition_point(|(t, _)| *t <= pos);
        n.checked_sub(1)
    }
}

/// Parses LRC: `[mm:ss.xx]text`, possibly with several stamps on one line.
pub fn parse_lrc(text: &str) -> Lyrics {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let stamp = &r[..end];
            let Some((m, s)) = stamp.split_once(':') else { break };
            let (Ok(m), Ok(s)) = (m.parse::<f32>(), s.parse::<f32>()) else { break };
            stamps.push(m * 60.0 + s);
            rest = &r[end + 1..];
        }
        for t in stamps {
            lines.push((t, rest.trim().to_string()));
        }
    }
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    Lyrics { lines }
}

/// "Song - Remastered 2011" and "Song (feat. X)" search better as "Song".
fn simplify(title: &str) -> &str {
    let cut = [" - ", " (feat", " (with", " [feat"].iter().filter_map(|p| title.find(p)).min();
    cut.map_or(title, |i| &title[..i]).trim()
}

fn synced(v: &Value) -> Option<Lyrics> {
    let l = parse_lrc(v["syncedLyrics"].as_str()?);
    (!l.lines.is_empty()).then_some(l)
}

/// Looks the song up; `Ok(None)` when LRCLIB has no synced lyrics for it.
pub async fn fetch(http: &reqwest::Client, title: &str, artist: &str, album: &str, duration: u32) -> Result<Option<Lyrics>> {
    let dur = duration.to_string();
    let mut q = vec![("track_name", title), ("artist_name", artist)];
    if !album.is_empty() {
        q.push(("album_name", album));
    }
    if duration > 0 {
        q.push(("duration", &dur));
    }
    let r = http.get(format!("{API}/get")).query(&q).header("User-Agent", AGENT).send().await?;
    if r.status().is_success() {
        if let Some(l) = synced(&r.json().await?) {
            return Ok(Some(l));
        }
    }
    // No exact match: search, and take the closest length that has synced lyrics.
    let r = http
        .get(format!("{API}/search"))
        .query(&[("track_name", simplify(title)), ("artist_name", artist)])
        .header("User-Agent", AGENT)
        .send()
        .await?;
    if !r.status().is_success() {
        return Ok(None);
    }
    let hits: Vec<Value> = r.json().await.unwrap_or_default();
    let best = hits
        .iter()
        .filter(|h| h["syncedLyrics"].is_string())
        .min_by_key(|h| (h["duration"].as_f64().unwrap_or(0.0) - duration as f64).abs() as i64);
    Ok(best.and_then(synced))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc() {
        let l = parse_lrc("[ar:x]\n[00:01.50]one\n[00:03.00][00:10.00]two\n[00:05.25]\n");
        assert_eq!(l.lines, vec![(1.5, "one".into()), (3.0, "two".into()), (5.25, String::new()), (10.0, "two".into())]);
        assert_eq!(l.index_at(0.5), None);
        assert_eq!(l.index_at(3.2), Some(1));
        assert_eq!(l.index_at(99.0), Some(3));
    }

    #[test]
    fn simplifies_titles() {
        assert_eq!(simplify("Song - Remastered 2011"), "Song");
        assert_eq!(simplify("Song (feat. X)"), "Song");
        assert_eq!(simplify("euphoria"), "euphoria");
    }
}

#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore = "network"]
    async fn fetches_euphoria() {
        let l = super::fetch(&reqwest::Client::new(), "euphoria", "Kendrick Lamar", "euphoria", 383).await.unwrap().unwrap();
        println!("{} lines, first: {:?}", l.lines.len(), l.lines.iter().find(|(_, t)| !t.is_empty()));
        assert!(l.lines.len() > 20);
    }
}
