//! Lyrics from LRCLIB (https://lrclib.net), fetched over WinHTTP on a worker thread.
//! A request sends the track's artist, title, album and length to that server. The thread
//! sleeps on a channel and only runs while the lyrics box asks for a track.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::http;

pub struct Request {
    /// Identifies the track; echoed back so the UI can drop stale answers.
    pub key: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration_ms: i64,
}

pub enum Body {
    /// (start in ms, text), sorted by time.
    Synced(Vec<(i64, String)>),
    Plain(Vec<String>),
    Message(&'static str),
}

pub struct Lyrics {
    pub key: String,
    pub body: Body,
}

static REQUESTS: OnceLock<Sender<Request>> = OnceLock::new();

/// Ask the worker for a track's lyrics. The answer arrives as a posted `Box<Lyrics>`.
pub fn request(r: Request) {
    if let Some(tx) = REQUESTS.get() {
        let _ = tx.send(r);
    }
}

/// Start the worker. `msg` is posted to `hwnd` with a `Box<Lyrics>` pointer in `lparam`.
pub fn spawn(hwnd: HWND, msg: u32) {
    let window = hwnd.0 as isize;
    let (tx, rx) = mpsc::channel::<Request>();
    let _ = REQUESTS.set(tx);
    let _ = std::thread::Builder::new().stack_size(512 * 1024).spawn(move || {
        while let Ok(mut req) = rx.recv() {
            // Only the newest request matters (the user skipped tracks while we waited).
            while let Ok(newer) = rx.try_recv() {
                req = newer;
            }
            let body = fetch(&req);
            let ptr = Box::into_raw(Box::new(Lyrics { key: req.key, body }));
            let sent = unsafe {
                PostMessageW(Some(HWND(window as *mut _)), msg, WPARAM(0), LPARAM(ptr as isize))
            };
            if sent.is_err() {
                drop(unsafe { Box::from_raw(ptr) });
            }
        }
    });
}

/// HTTPS GET to lrclib.net. Returns (status code, body).
fn get(path: &str) -> Result<(u32, String), ()> {
    http::request("GET", "lrclib.net", path, "", &[]).map(|r| (r.status, r.body))
}

/// Blocking lookup; also used by the `lyrics_probe` dev binary.
pub fn fetch(req: &Request) -> Body {
    let exact = format!(
        "/api/get?artist_name={}&track_name={}&album_name={}&duration={}",
        http::encode(&req.artist),
        http::encode(&req.title),
        http::encode(&req.album),
        req.duration_ms / 1000
    );
    let reply = match get(&exact) {
        Ok((200, body)) => Ok(body),
        // No exact match (album or length differ, the length is unknown) or the exact lookup
        // failed on the server (it answers 503 for some tracks): search instead.
        _ => {
            let search = format!(
                "/api/search?artist_name={}&track_name={}",
                http::encode(&req.artist),
                http::encode(&req.title)
            );
            match get(&search) {
                Ok((200, body)) => Ok(body),
                Ok((400..=499, _)) => return Body::Message("No lyrics found"),
                _ => Err(()),
            }
        }
    };
    match reply {
        Ok(body) => parse(&body),
        Err(()) => Body::Message("Couldn't load lyrics"),
    }
}

fn parse(body: &str) -> Body {
    if body.contains("\"instrumental\":true") {
        return Body::Message("Instrumental");
    }
    if let Some(lrc) = http::json_string(body, "syncedLyrics").filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(&lrc);
        if !lines.is_empty() {
            return Body::Synced(lines);
        }
    }
    if let Some(plain) = http::json_string(body, "plainLyrics").filter(|s| !s.trim().is_empty()) {
        return Body::Plain(plain.lines().map(|l| l.trim().to_string()).collect());
    }
    Body::Message("No lyrics found")
}

/// "[01:23.45] text", possibly with several leading timestamps. Other tags ("[ar:..]") are skipped.
fn parse_lrc(lrc: &str) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for line in lrc.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            match timestamp(&stripped[..end]) {
                Some(t) => times.push(t),
                None => break,
            }
            rest = stripped[end + 1..].trim_start();
        }
        for t in times {
            out.push((t, rest.trim().to_string()));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// "mm:ss" or "mm:ss.xx" / "mm:ss.xxx" to milliseconds.
fn timestamp(s: &str) -> Option<i64> {
    let (min, rest) = s.split_once(':')?;
    let (sec, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let ms = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()? * 100,
        2 => frac.parse::<i64>().ok()? * 10,
        _ => frac.get(..3)?.parse::<i64>().ok()?,
    };
    Some(min.parse::<i64>().ok()? * 60_000 + sec.parse::<i64>().ok()? * 1000 + ms)
}
