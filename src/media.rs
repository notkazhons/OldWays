//! Spotify now-playing state from Windows media controls (SMTC), read on a worker thread.
//! The thread sleeps on a channel; SMTC events and UI commands wake it. It posts a `NowPlaying`
//! to the window only when something the UI shows has changed.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Props,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Media::MediaPlaybackAutoRepeatMode;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::Result;

use crate::volume;

/// Album art is decoded once at this square size and scaled to the window when drawn.
pub const ART_SIZE: i32 = 160;

/// SMTC timestamps are 100 ns ticks; one millisecond is this many.
const TICKS_PER_MS: i64 = 10_000;
/// Ticks between 1601-01-01 (SMTC epoch) and 1970-01-01.
const EPOCH_DIFF_TICKS: i64 = 116_444_736_000_000_000;
/// A reported position this far from where we expected it counts as a seek.
const JUMP_MS: i64 = 1500;

pub struct Art {
    /// `ART_SIZE` x `ART_SIZE`, 32 bits per pixel, BGRA.
    pub bgra: Vec<u8>,
    /// Dominant vivid color (r, g, b), if the art has one.
    pub accent: Option<(u8, u8, u8)>,
    /// Three distinct main colors, most common first, for the flowing background.
    pub palette: [(u8, u8, u8); 3],
}

/// Three well-separated common colors: 8 levels per channel histogram, weighted toward
/// saturated pixels, taking the heaviest bin and then the heaviest bins far from those chosen.
/// Dark picks are lifted so the background shows them.
fn palette_of(bgra: &[u8]) -> [(u8, u8, u8); 3] {
    // Per bin: weight and weighted sums of r, g, b.
    let mut bins = vec![(0f32, 0f32, 0f32, 0f32); 512];
    for px in bgra.chunks_exact(4) {
        let (b, g, r) = (px[0], px[1], px[2]);
        let (max, min) = (r.max(g).max(b) as f32, r.min(g).min(b) as f32);
        let w = 1.0 + 2.0 * if max > 0.0 { (max - min) / max } else { 0.0 };
        let k = ((r >> 5) as usize) << 6 | ((g >> 5) as usize) << 3 | (b >> 5) as usize;
        let e = &mut bins[k];
        *e = (e.0 + w, e.1 + r as f32 * w, e.2 + g as f32 * w, e.3 + b as f32 * w);
    }
    let mean = |e: &(f32, f32, f32, f32)| [e.1 / e.0, e.2 / e.0, e.3 / e.0];
    let mut picked: Vec<[f32; 3]> = Vec::new();
    for _ in 0..3 {
        let far = |c: &[f32; 3]| {
            picked.iter().all(|p| (0..3).map(|i| (p[i] - c[i]).powi(2)).sum::<f32>() > 60.0 * 60.0)
        };
        let best = bins
            .iter()
            .filter(|e| e.0 > 0.0 && far(&mean(e)))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(e) = best {
            picked.push(mean(e));
        }
    }
    if picked.is_empty() {
        picked.push([90.0, 90.0, 110.0]);
    }
    let lift = |c: [f32; 3]| {
        let max = c[0].max(c[1]).max(c[2]).max(1.0);
        let k = (110.0 / max).clamp(1.0, 3.0);
        ((c[0] * k).min(255.0) as u8, (c[1] * k).min(255.0) as u8, (c[2] * k).min(255.0) as u8)
    };
    std::array::from_fn(|i| lift(picked[i.min(picked.len() - 1)]))
}

/// Most common vivid hue in the art: weight pixels by saturation, bucket by hue (12 buckets of
/// 30 degrees), and average the heaviest bucket. Greys and near-black/white pixels are ignored.
fn accent_of(bgra: &[u8]) -> Option<(u8, u8, u8)> {
    // Per bucket: total weight and weighted sums of r, g, b.
    let mut buckets = [(0f32, 0f32, 0f32, 0f32); 12];
    for px in bgra.chunks_exact(4) {
        let (b, g, r) = (px[0] as f32, px[1] as f32, px[2] as f32);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let (sat, val) = if max > 0.0 { ((max - min) / max, max / 255.0) } else { (0.0, 0.0) };
        if sat < 0.35 || !(0.25..=0.97).contains(&val) {
            continue;
        }
        let d = max - min;
        let hue = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } * 60.0;
        let w = sat * (1.0 - (val - 0.65).abs());
        let k = ((hue / 30.0) as usize).min(11);
        let e = &mut buckets[k];
        *e = (e.0 + w, e.1 + r * w, e.2 + g * w, e.3 + b * w);
    }
    let best = buckets.iter().max_by(|a, b| a.0.total_cmp(&b.0))?;
    // Needs a few percent of the image in one hue, or the art is effectively grey.
    if best.0 < (bgra.len() / 4) as f32 * 0.02 {
        return None;
    }
    Some(((best.1 / best.0) as u8, (best.2 / best.0) as u8, (best.3 / best.0) as u8))
}

/// What to do with the bitmap the UI already holds.
pub enum ArtUpdate {
    /// Same track: keep the current art.
    Keep,
    Set(Art),
    /// New track or no session without art: show the placeholder.
    Clear,
}

pub struct NowPlaying {
    /// A Spotify session exists.
    pub active: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: ArtUpdate,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
    pub can_toggle: bool,
    pub can_seek: bool,
    pub can_shuffle: bool,
    pub can_repeat: bool,
    pub shuffle: bool,
    /// 0 off, 1 repeat the track, 2 repeat the list.
    pub repeat: u8,
    /// Spotify's mixer volume 0..=100 and mute, or -1 when it has no audio session.
    pub volume: i32,
    pub muted: bool,
    /// Track length and the position at the moment this was read, in milliseconds.
    /// While playing the UI advances the position itself from its own clock.
    pub dur_ms: i64,
    pub pos_ms: i64,
}

impl NowPlaying {
    fn empty(active: bool) -> Self {
        Self {
            active,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            art: ArtUpdate::Clear,
            playing: false,
            can_prev: false,
            can_next: false,
            can_toggle: false,
            can_seek: false,
            can_shuffle: false,
            can_repeat: false,
            shuffle: false,
            repeat: 0,
            volume: -1,
            muted: false,
            dur_ms: 0,
            pos_ms: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Command {
    Toggle,
    Next,
    Prev,
    /// Absolute position in milliseconds.
    Seek(i64),
    Shuffle(bool),
    /// 0 off, 1 track, 2 list.
    Repeat(u8),
    /// Mixer volume 0..=100.
    Volume(i32),
    Mute(bool),
}

enum Msg {
    Refresh,
    Command(Command),
    /// The UI dropped the album art (window hidden); send it again.
    ResendArt,
}

static CONTROL: OnceLock<Sender<Msg>> = OnceLock::new();

/// Ask the worker to decode and post the current album art again.
pub fn resend_art() {
    if let Some(tx) = CONTROL.get() {
        let _ = tx.send(Msg::ResendArt);
    }
}

/// Ask the worker to run a playback command on Spotify.
pub fn command(c: Command) {
    if let Some(tx) = CONTROL.get() {
        let _ = tx.send(Msg::Command(c));
    }
}

/// Event registrations on the current session; removed when the session goes away.
struct Subscription {
    session: Session,
    media: i64,
    playback: i64,
    timeline: i64,
}

impl Subscription {
    fn new(session: Session, tx: &Sender<Msg>) -> Result<Self> {
        let t = tx.clone();
        let media = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Msg::Refresh);
            Ok(())
        }))?;
        let t = tx.clone();
        let playback = session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Msg::Refresh);
            Ok(())
        }))?;
        // Spotify fires this every few seconds while playing; the worker only forwards it to
        // the UI when the position jumped (a seek), so it costs no redraw.
        let t = tx.clone();
        let timeline = session.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Msg::Refresh);
            Ok(())
        }))?;
        Ok(Self { session, media, playback, timeline })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.session.RemoveMediaPropertiesChanged(self.media);
        let _ = self.session.RemovePlaybackInfoChanged(self.playback);
        let _ = self.session.RemoveTimelinePropertiesChanged(self.timeline);
    }
}

fn find_spotify(manager: &Manager) -> Result<Option<Session>> {
    for session in manager.GetSessions()? {
        if session.SourceAppUserModelId()?.to_string().to_lowercase().contains("spotify") {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

fn load_art(props: &Props) -> Option<Art> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
    let decoder = BitmapDecoder::CreateAsync(&stream).ok()?.join().ok()?;
    let transform = BitmapTransform::new().ok()?;
    transform.SetScaledWidth(ART_SIZE as u32).ok()?;
    transform.SetScaledHeight(ART_SIZE as u32).ok()?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant).ok()?;
    let data = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Straight,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
        .ok()?
        .join()
        .ok()?;
    let bgra = data.DetachPixelData().ok()?.to_vec();
    if bgra.len() != (ART_SIZE * ART_SIZE * 4) as usize {
        return None;
    }
    let accent = accent_of(&bgra);
    let palette = palette_of(&bgra);
    Some(Art { bgra, accent, palette })
}

fn post(window: isize, msg: u32, np: NowPlaying) {
    let ptr = Box::into_raw(Box::new(np));
    let sent = unsafe {
        PostMessageW(Some(HWND(window as *mut _)), msg, WPARAM(0), LPARAM(ptr as isize))
    };
    if sent.is_err() {
        // Window is gone; reclaim the allocation.
        drop(unsafe { Box::from_raw(ptr) });
    }
}

/// Start the worker. `msg` is posted to `hwnd` with a `Box<NowPlaying>` pointer in `lparam`;
/// the window procedure takes ownership.
pub fn spawn(hwnd: HWND, msg: u32) {
    let window = hwnd.0 as isize;
    let (tx, rx) = mpsc::channel::<Msg>();
    let _ = CONTROL.set(tx.clone());
    let _ = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let _ = run(window, msg, tx, rx);
        });
}

/// What the UI shows. Used to skip posts when nothing visible changed.
#[derive(PartialEq, Default)]
struct Key {
    active: bool,
    title: String,
    artist: String,
    has_art: bool,
    playing: bool,
    can_prev: bool,
    can_next: bool,
    can_toggle: bool,
    can_seek: bool,
    can_shuffle: bool,
    can_repeat: bool,
    shuffle: bool,
    repeat: u8,
    volume: i32,
    muted: bool,
    dur_ms: i64,
}

/// Runs a command. Returns true when the message asks for the art to be sent again.
fn handle(manager: &Manager, msg: Msg) -> bool {
    match msg {
        Msg::Refresh => {}
        Msg::ResendArt => return true,
        // Volume is the audio session, not the media session.
        Msg::Command(Command::Volume(v)) => volume::set(v),
        Msg::Command(Command::Mute(m)) => volume::set_mute(m),
        Msg::Command(c) => {
            if let Ok(Some(session)) = find_spotify(manager) {
                let op = match c {
                    Command::Toggle => session.TryTogglePlayPauseAsync(),
                    Command::Next => session.TrySkipNextAsync(),
                    Command::Prev => session.TrySkipPreviousAsync(),
                    Command::Seek(ms) => session.TryChangePlaybackPositionAsync(ms * TICKS_PER_MS),
                    Command::Shuffle(on) => session.TryChangeShuffleActiveAsync(on),
                    Command::Repeat(mode) => session.TryChangeAutoRepeatModeAsync(match mode {
                        1 => MediaPlaybackAutoRepeatMode::Track,
                        2 => MediaPlaybackAutoRepeatMode::List,
                        _ => MediaPlaybackAutoRepeatMode::None,
                    }),
                    Command::Volume(_) | Command::Mute(_) => return false,
                };
                let _ = op.and_then(|o| o.join());
            }
        }
    }
    false
}

fn now_ticks() -> i64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_nanos() / 100) as i64 + EPOCH_DIFF_TICKS
}

fn snapshot(session: &Session, last: &Key) -> Result<(NowPlaying, Key)> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let title = props.Title()?.to_string();
    let artist = props.Artist()?.to_string();
    let album = props.AlbumTitle()?.to_string();
    let info = session.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let playing = info.PlaybackStatus()? == Status::Playing;
    let can_prev = controls.IsPreviousEnabled()?;
    let can_next = controls.IsNextEnabled()?;
    let can_toggle = controls.IsPlayEnabled()? || controls.IsPauseEnabled()?;
    let can_seek = controls.IsPlaybackPositionEnabled()?;
    let can_shuffle = controls.IsShuffleEnabled()?;
    let can_repeat = controls.IsRepeatEnabled()?;
    let shuffle = info.IsShuffleActive().and_then(|v| v.Value()).unwrap_or(false);
    let repeat = match info.AutoRepeatMode().and_then(|v| v.Value()) {
        Ok(MediaPlaybackAutoRepeatMode::Track) => 1,
        Ok(MediaPlaybackAutoRepeatMode::List) => 2,
        _ => 0,
    };
    let (volume, muted) = volume::get().unwrap_or((-1, false));

    // The reported position is a snapshot taken at LastUpdatedTime; bring it up to now.
    let tl = session.GetTimelineProperties()?;
    let start = tl.StartTime()?.Duration;
    let end = tl.EndTime()?.Duration;
    let mut pos = tl.Position()?.Duration;
    let updated = tl.LastUpdatedTime()?.UniversalTime;
    if playing && updated > EPOCH_DIFF_TICKS {
        pos += now_ticks() - updated;
    }
    let dur_ms = ((end - start) / TICKS_PER_MS).max(0);
    let pos_ms = ((pos - start) / TICKS_PER_MS).clamp(0, dur_ms);

    // Decode art only for a new track, or when it was missing last time.
    let need_art = !last.active || last.title != title || last.artist != artist || !last.has_art;
    let art = if !need_art {
        ArtUpdate::Keep
    } else {
        load_art(&props).map_or(ArtUpdate::Clear, ArtUpdate::Set)
    };
    let has_art = !matches!(art, ArtUpdate::Clear);
    let key = Key {
        active: true,
        title: title.clone(),
        artist: artist.clone(),
        has_art,
        playing,
        can_prev,
        can_next,
        can_toggle,
        can_seek,
        can_shuffle,
        can_repeat,
        shuffle,
        repeat,
        volume,
        muted,
        dur_ms,
    };
    let np = NowPlaying {
        active: true,
        title,
        artist,
        album,
        art,
        playing,
        can_prev,
        can_next,
        can_toggle,
        can_seek,
        can_shuffle,
        can_repeat,
        shuffle,
        repeat,
        volume,
        muted,
        dur_ms,
        pos_ms,
    };
    Ok((np, key))
}

fn run(window: isize, msg: u32, tx: Sender<Msg>, rx: mpsc::Receiver<Msg>) -> Result<()> {
    let manager = Manager::RequestAsync()?.join()?;
    let t = tx.clone();
    manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = t.send(Msg::Refresh);
        Ok(())
    }))?;

    let mut sub: Option<Subscription> = None;
    let mut last = Key::default();
    // Position the UI is extrapolating from: (ms, when we sent it, was playing).
    let mut base: Option<(i64, Instant, bool)> = None;
    let mut first = true;
    loop {
        let (np, key) = match find_spotify(&manager)? {
            Some(session) => {
                let same = sub.as_ref().is_some_and(|s| {
                    s.session.SourceAppUserModelId().ok() == session.SourceAppUserModelId().ok()
                });
                if !same {
                    sub = Some(Subscription::new(session.clone(), &tx)?);
                }
                snapshot(&session, &last)
                    .unwrap_or_else(|_| (NowPlaying::empty(true), Key { active: true, ..Default::default() }))
            }
            None => {
                sub = None;
                (NowPlaying::empty(false), Key::default())
            }
        };

        // Did the position move somewhere the UI's own clock would not have taken it?
        let jumped = base.is_some_and(|(pos, at, was_playing)| {
            let expected = if was_playing { pos + at.elapsed().as_millis() as i64 } else { pos };
            (np.pos_ms - expected).abs() > JUMP_MS
        });
        if first || last != key || jumped {
            first = false;
            last = key;
            base = Some((np.pos_ms, Instant::now(), np.playing));
            post(window, msg, np);
        }
        // Block until the next SMTC event or command; no polling.
        // Spotify's audio session can appear a moment after its media session; until it does,
        // look again every couple of seconds instead of waiting for the next media event.
        let first_msg = if last.active && last.volume < 0 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return Ok(()),
            }
        };
        let mut resend = first_msg.is_some_and(|m| handle(&manager, m));
        while let Ok(m) = rx.try_recv() {
            resend |= handle(&manager, m);
        }
        if resend {
            // Pretend the art is missing so the next snapshot decodes and posts it.
            last.has_art = false;
        }
    }
}
