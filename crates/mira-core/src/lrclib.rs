//! LRCLIB (<https://lrclib.net>) client — the *only* network access in Mira.
//!
//! We query `/api/get` by track metadata and duration; when that misses we
//! fall back to `/api/search`. The JSON→[`Lyrics`] mapping is factored out so
//! it can be unit-tested without touching the network.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::lyrics::{Lyrics, LyricsSource};
use crate::model::{NowPlaying, MICROS_PER_SEC};

const API_BASE: &str = "https://lrclib.net";
const USER_AGENT: &str = concat!(
    "Mira/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/antonio-lucian-popa/mira-notch-assistant)"
);

/// What we know about the track we want lyrics for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackQuery {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Track length in whole seconds; LRCLIB matches within ±2s.
    pub duration_secs: Option<i64>,
}

impl TrackQuery {
    pub fn new(title: impl Into<String>, artist: impl Into<String>) -> Self {
        TrackQuery {
            title: title.into(),
            artist: artist.into(),
            ..Default::default()
        }
    }

    /// Build a lookup for the track a media source reports, cleaning up the
    /// title/artist first (see [`clean_track`]).
    pub fn for_now_playing(np: &NowPlaying) -> Self {
        let (title, artist) = clean_track(&np.title, &np.artist);
        TrackQuery {
            title,
            artist,
            album: np.album.clone(),
            duration_secs: (np.length_micros > 0).then_some(np.length_micros / MICROS_PER_SEC),
        }
    }
}

/// Normalise a now-playing title/artist for lyric lookup. Browser/YouTube
/// tracks arrive as a messy single title ("Artist - Song (Official Video)")
/// with no artist: strip common video-title noise and, when the artist is
/// empty, split an "Artist - Song" title.
pub fn clean_track(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut title = strip_noise(raw_title);
    let mut artist = strip_noise(raw_artist);
    // Drop a channel "- Topic" suffix that YouTube Music adds to the artist.
    if let Some(stripped) = artist.strip_suffix("- Topic") {
        artist = stripped.trim().to_string();
    }
    if artist.is_empty() {
        if let Some((a, t)) = title.split_once(" - ") {
            artist = a.trim().to_string();
            title = t.trim().to_string();
        }
    }
    (title, artist)
}

/// Remove bracketed/segment noise such as "(Official Video)", "[Lyrics]",
/// "(feat. X)", "| NPR", trailing "- YouTube", and collapse whitespace.
fn strip_noise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    for ch in s.chars() {
        match ch {
            '(' => depth_paren += 1,
            ')' => depth_paren = (depth_paren - 1).max(0),
            '[' => depth_brack += 1,
            ']' => depth_brack = (depth_brack - 1).max(0),
            _ if depth_paren == 0 && depth_brack == 0 => out.push(ch),
            _ => {}
        }
    }
    // Cut anything after a pipe (e.g. "Song | Live at ...").
    if let Some(idx) = out.find('|') {
        out.truncate(idx);
    }
    let out = out
        .trim()
        .trim_end_matches("- YouTube")
        .trim_end_matches("- Topic")
        .trim();
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Raw shape of an LRCLIB record (`/api/get` and items from `/api/search`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LrclibRecord {
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    #[serde(default)]
    pub synced_lyrics: Option<String>,
}

impl LrclibRecord {
    /// Convert a record into [`Lyrics`], preferring synced over plain.
    /// Returns [`Error::LyricsNotFound`] for instrumental or empty records.
    pub fn into_lyrics(self) -> Result<Lyrics> {
        if self.instrumental {
            return Err(Error::LyricsNotFound);
        }
        if let Some(synced) = self.synced_lyrics.filter(|s| !s.trim().is_empty()) {
            let lyrics = Lyrics::parse_lrc(&synced, LyricsSource::Lrclib);
            if !lyrics.is_empty() {
                return Ok(lyrics);
            }
        }
        if let Some(plain) = self.plain_lyrics.filter(|s| !s.trim().is_empty()) {
            let lyrics = Lyrics::from_plain(&plain, LyricsSource::Lrclib);
            if !lyrics.is_empty() {
                return Ok(lyrics);
            }
        }
        Err(Error::LyricsNotFound)
    }
}

/// Thin async client around the LRCLIB HTTP API.
#[derive(Clone)]
pub struct LrclibClient {
    http: reqwest::Client,
    base: String,
}

impl LrclibClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
        Ok(LrclibClient {
            http,
            base: API_BASE.to_string(),
        })
    }

    /// Override the base URL (for testing against a local mock server).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Fetch lyrics for a track: exact `/api/get` first, then `/api/search`.
    pub async fn fetch(&self, q: &TrackQuery) -> Result<Lyrics> {
        if let Ok(lyrics) = self.get(q).await {
            return Ok(lyrics);
        }
        self.search(q).await
    }

    async fn get(&self, q: &TrackQuery) -> Result<Lyrics> {
        let mut params: Vec<(&str, String)> = vec![
            ("track_name", q.title.clone()),
            ("artist_name", q.artist.clone()),
        ];
        if !q.album.is_empty() {
            params.push(("album_name", q.album.clone()));
        }
        if let Some(d) = q.duration_secs {
            params.push(("duration", d.to_string()));
        }

        let url = format!("{}/api/get", self.base);
        let resp = self.http.get(&url).query(&params).send().await?;
        if !resp.status().is_success() {
            return Err(Error::LyricsNotFound);
        }
        let record: LrclibRecord = resp.json().await?;
        record.into_lyrics()
    }

    async fn search(&self, q: &TrackQuery) -> Result<Lyrics> {
        let params: Vec<(&str, String)> = vec![
            ("track_name", q.title.clone()),
            ("artist_name", q.artist.clone()),
        ];
        let url = format!("{}/api/search", self.base);
        let resp = self.http.get(&url).query(&params).send().await?;
        if !resp.status().is_success() {
            return Err(Error::LyricsNotFound);
        }
        let records: Vec<LrclibRecord> = resp.json().await?;
        // Take the first record that yields usable lyrics.
        records
            .into_iter()
            .find_map(|r| r.into_lyrics().ok())
            .ok_or(Error::LyricsNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_synced_record_to_timed_lyrics() {
        let json = r#"{
            "instrumental": false,
            "plainLyrics": "Hello\nWorld",
            "syncedLyrics": "[00:01.00]Hello\n[00:02.00]World"
        }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();
        let lyrics = record.into_lyrics().unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 2);
    }

    #[test]
    fn falls_back_to_plain_when_no_synced() {
        let json = r#"{ "instrumental": false, "plainLyrics": "Just text", "syncedLyrics": null }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();
        let lyrics = record.into_lyrics().unwrap();
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "Just text");
    }

    #[test]
    fn instrumental_has_no_lyrics() {
        let json = r#"{ "instrumental": true, "plainLyrics": null, "syncedLyrics": null }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();
        assert!(matches!(record.into_lyrics(), Err(Error::LyricsNotFound)));
    }

    #[test]
    fn empty_strings_count_as_missing() {
        let json = r#"{ "instrumental": false, "plainLyrics": "   ", "syncedLyrics": "" }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();
        assert!(matches!(record.into_lyrics(), Err(Error::LyricsNotFound)));
    }

    #[test]
    fn tolerates_extra_unknown_fields() {
        let json = r#"{
            "id": 42,
            "trackName": "X",
            "artistName": "Y",
            "duration": 180,
            "instrumental": false,
            "plainLyrics": "a",
            "syncedLyrics": null
        }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();
        assert!(record.into_lyrics().is_ok());
    }

    #[test]
    fn cleans_browser_video_titles() {
        assert_eq!(
            clean_track("Daft Punk - One More Time (Official Video)", ""),
            ("One More Time".to_string(), "Daft Punk".to_string())
        );
        assert_eq!(
            clean_track("Song [Lyrics] | Live", "Band - Topic"),
            ("Song".to_string(), "Band".to_string())
        );
    }
}
