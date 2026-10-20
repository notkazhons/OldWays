//! Spotify now-playing state from Windows media controls (SMTC), read on a worker thread.
//! The thread sleeps on a channel; SMTC events and UI commands wake it. It posts a `NowPlaying`
//! to the window only when something the UI shows has changed.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::Result;

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
    pub art: ArtUpdate,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
    pub can_toggle: bool,
    pub can_seek: bool,
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
            art: ArtUpdate::Clear,
            playing: false,
            can_prev: false,
            can_next: false,
            can_toggle: false,
            can_seek: false,
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
}

enum Msg {
    Refresh,
    Command(Command),
}

static CONTROL: OnceLock<Sender<Msg>> = OnceLock::new();

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
    (bgra.len() == (ART_SIZE * ART_SIZE * 4) as usize).then_some(Art { bgra })
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
    dur_ms: i64,
}

fn handle(manager: &Manager, msg: Msg) {
    if let Msg::Command(c) = msg {
        if let Ok(Some(session)) = find_spotify(manager) {
            let op = match c {
                Command::Toggle => session.TryTogglePlayPauseAsync(),
                Command::Next => session.TrySkipNextAsync(),
                Command::Prev => session.TrySkipPreviousAsync(),
                Command::Seek(ms) => session.TryChangePlaybackPositionAsync(ms * TICKS_PER_MS),
            };
            let _ = op.and_then(|o| o.join());
        }
    }
}

fn now_ticks() -> i64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_nanos() / 100) as i64 + EPOCH_DIFF_TICKS
}

fn snapshot(session: &Session, last: &Key) -> Result<(NowPlaying, Key)> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let title = props.Title()?.to_string();
    let artist = props.Artist()?.to_string();
    let info = session.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let playing = info.PlaybackStatus()? == Status::Playing;
    let can_prev = controls.IsPreviousEnabled()?;
    let can_next = controls.IsNextEnabled()?;
    let can_toggle = controls.IsPlayEnabled()? || controls.IsPauseEnabled()?;
    let can_seek = controls.IsPlaybackPositionEnabled()?;

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
        dur_ms,
    };
    let np = NowPlaying {
        active: true,
        title,
        artist,
        art,
        playing,
        can_prev,
        can_next,
        can_toggle,
        can_seek,
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
        match rx.recv() {
            Ok(m) => handle(&manager, m),
            Err(_) => return Ok(()),
        }
        while let Ok(m) = rx.try_recv() {
            handle(&manager, m);
        }
    }
}
