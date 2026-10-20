use std::sync::mpsc::{self, Sender};

use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::core::Result;

/// Event registrations on one session; removed when the session goes away.
struct Subscription {
    session: Session,
    media: i64,
    playback: i64,
    timeline: i64,
}

impl Subscription {
    fn new(session: Session, tx: &Sender<()>) -> Result<Self> {
        let t = tx.clone();
        let media = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(());
            Ok(())
        }))?;
        let t = tx.clone();
        let playback = session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(());
            Ok(())
        }))?;
        let t = tx.clone();
        let timeline = session.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(());
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

fn describe(session: &Session) -> Result<String> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let status = match session.GetPlaybackInfo()?.PlaybackStatus()? {
        Status::Playing => "playing",
        Status::Paused => "paused",
        Status::Stopped => "stopped",
        _ => "other",
    };
    let timeline = session.GetTimelineProperties()?;
    let secs = |d: windows::Foundation::TimeSpan| d.Duration / 10_000_000;
    let (pos, end) = (secs(timeline.Position()?), secs(timeline.EndTime()?));
    Ok(format!(
        "{status} | {} - {} | {} | {}:{:02} / {}:{:02}",
        props.Title()?,
        props.Artist()?,
        props.AlbumTitle()?,
        pos / 60,
        pos % 60,
        end / 60,
        end % 60,
    ))
}

fn main() -> Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let manager = Manager::RequestAsync()?.join()?;

    let t = tx.clone();
    manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = t.send(());
        Ok(())
    }))?;

    let mut sub: Option<Subscription> = None;
    let mut last = String::new();
    loop {
        let line = match find_spotify(&manager)? {
            Some(session) => {
                let same = sub.as_ref().is_some_and(|s| {
                    s.session.SourceAppUserModelId().ok() == session.SourceAppUserModelId().ok()
                });
                if !same {
                    sub = Some(Subscription::new(session.clone(), &tx)?);
                }
                describe(&session).unwrap_or_else(|e| format!("error: {e}"))
            }
            None => {
                sub = None;
                "Spotify not running".to_string()
            }
        };
        if line != last {
            println!("{line}");
            last = line;
        }
        // Block until the next SMTC event; no polling.
        if rx.recv().is_err() {
            return Ok(());
        }
        while rx.try_recv().is_ok() {}
    }
}
