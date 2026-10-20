//! Lyrics from LRCLIB (https://lrclib.net), fetched over WinHTTP on a worker thread.
//! A request sends the track's artist, title, album and length to that server. The thread
//! sleeps on a channel and only runs while the lyrics box asks for a track.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{PCWSTR, w};

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

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Blocking lookup; also used by the `lyrics_probe` dev binary.
pub fn fetch(req: &Request) -> Body {
    let exact = format!(
        "/api/get?artist_name={}&track_name={}&album_name={}&duration={}",
        encode(&req.artist),
        encode(&req.title),
        encode(&req.album),
        req.duration_ms / 1000
    );
    let reply = match get(&exact) {
        Ok((200, body)) => Ok(body),
        // No exact match (album or length differ, the length is unknown) or the exact lookup
        // failed on the server (it answers 503 for some tracks): search instead.
        _ => {
            let search = format!(
                "/api/search?artist_name={}&track_name={}",
                encode(&req.artist),
                encode(&req.title)
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
    if let Some(lrc) = json_string(body, "syncedLyrics").filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(&lrc);
        if !lines.is_empty() {
            return Body::Synced(lines);
        }
    }
    if let Some(plain) = json_string(body, "plainLyrics").filter(|s| !s.trim().is_empty()) {
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

/// Value of the first `"key": "..."` string in a JSON document; `None` for null or absent.
fn json_string(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let after = body[body.find(&pat)? + pat.len()..].trim_start();
    let mut chars = after.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hi = hex4(&mut chars)?;
                    if (0xD800..0xDC00).contains(&hi) {
                        // Surrogate pair: expect "\uXXXX" next.
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        let lo = hex4(&mut chars)?;
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo.checked_sub(0xDC00)?);
                        out.push(char::from_u32(cp)?);
                    } else {
                        out.push(char::from_u32(hi)?);
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

fn hex4(chars: &mut std::str::Chars) -> Option<u32> {
    let mut v = 0;
    for _ in 0..4 {
        v = v * 16 + chars.next()?.to_digit(16)?;
    }
    Some(v)
}

/// RAII wrapper so every exit path closes the WinHTTP handle.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// HTTPS GET to lrclib.net. Returns (status code, body).
fn get(path: &str) -> std::result::Result<(u32, String), ()> {
    unsafe {
        let session = Handle(WinHttpOpen(
            w!("music-player-widget/0.1 (https://github.com/notkazhons/music-player-widget)"),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(());
        }
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 10_000);
        let connect = Handle(WinHttpConnect(session.0, w!("lrclib.net"), 443, 0));
        if connect.0.is_null() {
            return Err(());
        }
        let wide_path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(wide_path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(());
        }
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|_| ())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|_| ())?;

        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|_| ())?;

        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|_| ())?;
            if available == 0 {
                break;
            }
            let mut buf = vec![0u8; available as usize];
            let mut read = 0u32;
            WinHttpReadData(request.0, buf.as_mut_ptr() as *mut c_void, available, &mut read).map_err(|_| ())?;
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > 2 * 1024 * 1024 {
                return Err(());
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}
