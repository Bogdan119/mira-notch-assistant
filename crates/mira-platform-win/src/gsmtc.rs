//! GSMTC media source and control.
//!
//! The Global System Media Transport Controls session manager tracks every app
//! that publishes media sessions (Spotify, browsers, the Media Player app, …)
//! and nominates a "current" one — the same session the Windows media flyout
//! shows. We follow that session and react to its `PlaybackInfoChanged`,
//! `MediaPropertiesChanged` and `TimelinePropertiesChanged` events — never poll.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::mpsc;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinStatus,
};
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::core::Interface;

use mira_core::error::{Error, Result};
use mira_core::event::{AppEvent, EventBus};
use mira_core::model::{MediaCapabilities, NowPlaying, PlaybackStatus};
use mira_core::source::{MediaControl, MediaSource, Source};

/// Shared handle to the session the source currently follows.
type CurrentSession = Arc<Mutex<Option<Session>>>;

/// Media source. Created with [`new_gsmtc`], alongside its control.
pub struct GsmtcSource {
    current: CurrentSession,
}

/// Control surface that targets whatever session the source currently tracks.
#[derive(Clone)]
pub struct GsmtcControl {
    current: CurrentSession,
}

/// Build a paired media source + control sharing the current session.
pub fn new_gsmtc() -> (GsmtcSource, GsmtcControl) {
    let current: CurrentSession = Arc::new(Mutex::new(None));
    (
        GsmtcSource {
            current: current.clone(),
        },
        GsmtcControl { current },
    )
}

/// What a WinRT callback tells the source loop.
enum Signal {
    /// The manager's current session (or the session list) changed.
    Session,
    /// The followed session (tagged with its generation) changed something.
    Playback(u64),
    Media(u64),
}

/// Track metadata that only changes on `MediaPropertiesChanged`; cached so
/// playback/timeline events don't refetch it.
#[derive(Clone, Default)]
struct Meta {
    title: String,
    artist: String,
    album: String,
    art_path: Option<String>,
}

impl MediaSource for GsmtcSource {}

#[async_trait]
impl Source for GsmtcSource {
    fn name(&self) -> &'static str {
        "gsmtc"
    }

    async fn run(self: Box<Self>, bus: EventBus) -> Result<()> {
        let manager = Manager::RequestAsync().map_err(err)?.await.map_err(err)?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Signal>();

        {
            let tx = tx.clone();
            manager
                .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = tx.send(Signal::Session);
                    Ok(())
                }))
                .map_err(err)?;
        }
        {
            let tx = tx.clone();
            manager
                .SessionsChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = tx.send(Signal::Session);
                    Ok(())
                }))
                .map_err(err)?;
        }

        let art_dir = art_dir();
        let mut generation: u64 = 0;
        // (session, playback token, media token, timeline token)
        let mut attached: Option<(Session, i64, i64, i64)> = None;
        let mut meta = Meta::default();
        let mut last_art_hash: Option<u64> = None;
        let mut need_attach = true;

        loop {
            if need_attach {
                need_attach = false;
                let session = manager.GetCurrentSession().ok();
                let same = match (&attached, &session) {
                    (Some((a, ..)), Some(b)) => a == b,
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    if let Some((old, p, m, t)) = attached.take() {
                        let _ = old.RemovePlaybackInfoChanged(p);
                        let _ = old.RemoveMediaPropertiesChanged(m);
                        let _ = old.RemoveTimelinePropertiesChanged(t);
                    }
                    generation += 1;
                    *self.current.lock().unwrap() = session.clone();
                    meta = Meta::default();
                    match session {
                        Some(s) => {
                            attached = Some(subscribe(&s, generation, &tx)?);
                            meta = read_meta(&s, &art_dir, &mut last_art_hash).await;
                            bus.publish(AppEvent::NowPlaying(read_now_playing(&s, &meta)));
                        }
                        None => bus.publish(AppEvent::NowPlaying(NowPlaying::default())),
                    }
                }
            }

            let Some(signal) = rx.recv().await else {
                break;
            };
            match signal {
                Signal::Session => need_attach = true,
                Signal::Playback(g) if g == generation => {
                    if let Some((s, ..)) = &attached {
                        bus.publish(AppEvent::NowPlaying(read_now_playing(s, &meta)));
                    }
                }
                Signal::Media(g) if g == generation => {
                    if let Some((s, ..)) = &attached {
                        meta = read_meta(s, &art_dir, &mut last_art_hash).await;
                        bus.publish(AppEvent::NowPlaying(read_now_playing(s, &meta)));
                    }
                }
                // Stale event from a session we already left.
                _ => {}
            }
        }
        Ok(())
    }
}

fn subscribe(
    s: &Session,
    generation: u64,
    tx: &mpsc::UnboundedSender<Signal>,
) -> Result<(Session, i64, i64, i64)> {
    let playback = {
        let tx = tx.clone();
        s.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Signal::Playback(generation));
            Ok(())
        }))
        .map_err(err)?
    };
    let media = {
        let tx = tx.clone();
        s.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Signal::Media(generation));
            Ok(())
        }))
        .map_err(err)?
    };
    let timeline = {
        let tx = tx.clone();
        s.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Signal::Playback(generation));
            Ok(())
        }))
        .map_err(err)?
    };
    Ok((s.clone(), playback, media, timeline))
}

async fn read_meta(s: &Session, art_dir: &Option<PathBuf>, last_hash: &mut Option<u64>) -> Meta {
    let Ok(op) = s.TryGetMediaPropertiesAsync() else {
        return Meta::default();
    };
    let Ok(props) = op.await else {
        return Meta::default();
    };
    let mut meta = Meta {
        title: props.Title().map(|h| h.to_string()).unwrap_or_default(),
        artist: props.Artist().map(|h| h.to_string()).unwrap_or_default(),
        album: props.AlbumTitle().map(|h| h.to_string()).unwrap_or_default(),
        art_path: None,
    };
    if let (Some(dir), Ok(thumb)) = (art_dir, props.Thumbnail()) {
        if let Ok(bytes) = read_thumbnail(&thumb).await {
            meta.art_path = save_art(dir, &bytes, last_hash);
        }
    }
    meta
}

async fn read_thumbnail(
    thumb: &windows::Storage::Streams::IRandomAccessStreamReference,
) -> windows::core::Result<Vec<u8>> {
    let stream = thumb.OpenReadAsync()?.await?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Ok(Vec::new());
    }
    let reader = DataReader::CreateDataReader(&stream.cast::<IInputStream>()?)?;
    let loaded = reader.LoadAsync(size)?.await?;
    let mut buf = vec![0u8; loaded as usize];
    reader.ReadBytes(&mut buf)?;
    Ok(buf)
}

/// Write cover art to the cache, named by a content hash so the UI notices a
/// change by path alone. The previous file is removed.
fn save_art(dir: &PathBuf, bytes: &[u8], last_hash: &mut Option<u64>) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let hash = fnv1a(bytes);
    let path = dir.join(format!("art-{hash:016x}.img"));
    if *last_hash != Some(hash) {
        if let Some(old) = last_hash.take() {
            let _ = std::fs::remove_file(dir.join(format!("art-{old:016x}.img")));
        }
        std::fs::write(&path, bytes).ok()?;
        *last_hash = Some(hash);
    }
    Some(path.to_string_lossy().into_owned())
}

fn art_dir() -> Option<PathBuf> {
    let dir = dirs::cache_dir()?.join("Mira").join("art");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn read_now_playing(s: &Session, meta: &Meta) -> NowPlaying {
    let info = s.GetPlaybackInfo().ok();
    let status = info
        .as_ref()
        .and_then(|i| i.PlaybackStatus().ok())
        .map(|st| match st {
            WinStatus::Playing => PlaybackStatus::Playing,
            WinStatus::Paused => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        })
        .unwrap_or_default();
    let caps = info
        .as_ref()
        .and_then(|i| i.Controls().ok())
        .map(|c| MediaCapabilities {
            can_play: c.IsPlayEnabled().unwrap_or(false)
                || c.IsPlayPauseToggleEnabled().unwrap_or(false),
            can_pause: c.IsPauseEnabled().unwrap_or(false)
                || c.IsPlayPauseToggleEnabled().unwrap_or(false),
            can_go_next: c.IsNextEnabled().unwrap_or(false),
            can_go_previous: c.IsPreviousEnabled().unwrap_or(false),
            can_seek: c.IsPlaybackPositionEnabled().unwrap_or(false),
        })
        .unwrap_or_default();

    let (position_micros, length_micros) = timeline_micros(s, status.is_playing());

    NowPlaying {
        status,
        title: meta.title.clone(),
        artist: meta.artist.clone(),
        album: meta.album.clone(),
        art_path: meta.art_path.clone(),
        length_micros,
        position_micros,
        caps,
        player_id: s
            .SourceAppUserModelId()
            .map(|h| h.to_string())
            .unwrap_or_default(),
        sampled_at: Some(Instant::now()),
    }
}

/// `(position, length)` in microseconds. GSMTC reports the position as of
/// `LastUpdatedTime`, so while playing we extrapolate to "now".
fn timeline_micros(s: &Session, playing: bool) -> (i64, i64) {
    let Ok(t) = s.GetTimelineProperties() else {
        return (0, 0);
    };
    // TimeSpan / DateTime are in 100 ns ticks.
    let start = t.StartTime().map(|d| d.Duration).unwrap_or(0);
    let end = t.EndTime().map(|d| d.Duration).unwrap_or(0);
    let mut pos = t.Position().map(|d| d.Duration).unwrap_or(0) - start;
    if playing {
        if let Ok(updated) = t.LastUpdatedTime() {
            let elapsed = now_filetime() - updated.UniversalTime;
            if elapsed > 0 {
                pos += elapsed;
            }
        }
    }
    let len = (end - start).max(0);
    if len > 0 {
        pos = pos.clamp(0, len);
    }
    (pos.max(0) / 10, len / 10)
}

fn now_filetime() -> i64 {
    let ft = unsafe { GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

impl GsmtcControl {
    fn session(&self) -> Result<Session> {
        self.current
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::other("no active media session"))
    }
}

#[async_trait]
impl MediaControl for GsmtcControl {
    async fn play_pause(&self) -> Result<()> {
        let op = self.session()?.TryTogglePlayPauseAsync().map_err(err)?;
        op.await.map(|_| ()).map_err(err)
    }

    async fn next(&self) -> Result<()> {
        let op = self.session()?.TrySkipNextAsync().map_err(err)?;
        op.await.map(|_| ()).map_err(err)
    }

    async fn previous(&self) -> Result<()> {
        let op = self.session()?.TrySkipPreviousAsync().map_err(err)?;
        op.await.map(|_| ()).map_err(err)
    }

    async fn seek(&self, offset_micros: i64) -> Result<()> {
        let s = self.session()?;
        let playing = s
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .map(|st| st == WinStatus::Playing)
            .unwrap_or(false);
        let (pos, _) = timeline_micros(&s, playing);
        self.set_position((pos + offset_micros).max(0)).await
    }

    async fn set_position(&self, micros: i64) -> Result<()> {
        let s = self.session()?;
        let start = s
            .GetTimelineProperties()
            .and_then(|t| t.StartTime())
            .map(|d| d.Duration)
            .unwrap_or(0);
        let op = s
            .TryChangePlaybackPositionAsync(start + micros * 10)
            .map_err(err)?;
        op.await.map(|_| ()).map_err(err)
    }
}

fn err(e: windows::core::Error) -> Error {
    Error::other(format!("gsmtc: {e}"))
}
