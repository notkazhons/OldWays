//! Spotify now-playing state from Windows media controls (SMTC), read on a worker thread.
//! The thread sleeps on a channel; SMTC events wake it. It posts a `NowPlaying` to the window
//! only when something the UI shows has changed.

use std::sync::mpsc::{self, Sender};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Props,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::Result;

/// Album art is decoded once at this square size and scaled to the window when drawn.
pub const ART_SIZE: i32 = 160;

pub struct Art {
    /// `ART_SIZE` x `ART_SIZE`, 32 bits per pixel, BGRA.
    pub bgra: Vec<u8>,
}

pub struct NowPlaying {
    /// A Spotify session exists.
    pub active: bool,
    pub title: String,
    pub artist: String,
    pub art: Option<Art>,
}

/// Event registration on the current session; removed when the session goes away.
struct Subscription {
    session: Session,
    token: i64,
}

impl Subscription {
    fn new(session: Session, tx: &Sender<()>) -> Result<Self> {
        let tx = tx.clone();
        let token = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.send(());
            Ok(())
        }))?;
        Ok(Self { session, token })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.session.RemoveMediaPropertiesChanged(self.token);
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
    let _ = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let _ = run(window, msg);
        });
}

/// (active, title, artist, has art): what the UI shows.
type Key = (bool, String, String, bool);

fn run(window: isize, msg: u32) -> Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let manager = Manager::RequestAsync()?.join()?;
    let t = tx.clone();
    manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = t.send(());
        Ok(())
    }))?;

    let mut sub: Option<Subscription> = None;
    let mut last: Option<Key> = None;
    loop {
        let (np, key) = match find_spotify(&manager)? {
            Some(session) => {
                let same = sub.as_ref().is_some_and(|s| {
                    s.session.SourceAppUserModelId().ok() == session.SourceAppUserModelId().ok()
                });
                if !same {
                    sub = Some(Subscription::new(session.clone(), &tx)?);
                }
                match session.TryGetMediaPropertiesAsync().and_then(|op| op.join()) {
                    Ok(props) => {
                        let title = props.Title()?.to_string();
                        let artist = props.Artist()?.to_string();
                        // Decode art only for a new track, or when it was missing last time.
                        let need_art = last
                            .as_ref()
                            .is_none_or(|l| !l.0 || l.1 != title || l.2 != artist || !l.3);
                        let art = if need_art { load_art(&props) } else { None };
                        let has_art = if need_art { art.is_some() } else { true };
                        let key = (true, title.clone(), artist.clone(), has_art);
                        (NowPlaying { active: true, title, artist, art }, key)
                    }
                    Err(_) => (NowPlaying { active: true, title: String::new(), artist: String::new(), art: None },
                        (true, String::new(), String::new(), false)),
                }
            }
            None => {
                sub = None;
                (
                    NowPlaying { active: false, title: String::new(), artist: String::new(), art: None },
                    (false, String::new(), String::new(), false),
                )
            }
        };
        if last.as_ref() != Some(&key) {
            last = Some(key);
            post(window, msg, np);
        }
        // Block until the next SMTC event; no polling.
        if rx.recv().is_err() {
            return Ok(());
        }
        while rx.try_recv().is_ok() {}
    }
}
