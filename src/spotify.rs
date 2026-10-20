//! Spotify Web API, used only for the "like" (Liked Songs) heart. Everything else the widget
//! does needs no login. Sign-in is OAuth with PKCE against the user's own Spotify developer app
//! (client ID in `%APPDATA%\music-player-widget\spotify.txt`); the refresh token lives in the
//! Windows Credential Manager. Until the user connects, this thread never touches the network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};
use windows::Win32::Security::Cryptography::{
    BCRYPT_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, BCryptHash,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_SHOWNORMAL};
use windows::core::{PCWSTR, PWSTR, w};

use crate::http;

const API_HOST: &str = "api.spotify.com";
const ACCOUNTS_HOST: &str = "accounts.spotify.com";
const REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";
const SCOPES: &str = "user-library-read user-library-modify";

pub enum Event {
    /// Signed in (a refresh token is stored) or not.
    Connected(bool),
    /// Whether the track playing now is in Liked Songs. `None` when it could not be found out.
    Liked { key: String, id: String, liked: Option<bool> },
    /// Something the user should know, shown in the lyrics-box style message line.
    Notice(&'static str),
}

enum Job {
    Connect,
    Disconnect,
    Lookup { key: String },
    Set { key: String, id: String, liked: bool },
}

static JOBS: OnceLock<Sender<Job>> = OnceLock::new();

fn send(job: Job) {
    if let Some(tx) = JOBS.get() {
        let _ = tx.send(job);
    }
}

/// Start sign-in in the browser.
pub fn connect() {
    send(Job::Connect);
}

pub fn disconnect() {
    send(Job::Disconnect);
}

/// Find out whether the current track is liked. The answer arrives as `Event::Liked`.
pub fn lookup(key: String) {
    send(Job::Lookup { key });
}

/// Like or unlike a track by id.
pub fn set_liked(key: String, id: String, liked: bool) {
    send(Job::Set { key, id, liked });
}

fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("music-player-widget").join("spotify.txt"))
}

/// First non-comment line of the config file.
pub fn client_id() -> Option<String> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string)
}

/// Create the config file with instructions if needed and open it in the default editor.
pub fn open_config() {
    let Some(path) = config_path() else { return };
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            &path,
            "# Spotify client ID for the widget's like button.\r\n\
             # 1. Create an app at https://developer.spotify.com/dashboard\r\n\
             # 2. Add this redirect URI to it: http://127.0.0.1:8888/callback\r\n\
             # 3. Paste the app's Client ID on the line below, save, then choose\r\n\
             #    \"Connect Spotify\" in the widget's tray menu.\r\n",
        );
    }
    open(&path.display().to_string());
}

fn open(target: &str) {
    let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

// ---- Credential Manager -------------------------------------------------------------------

const CRED_TARGET: PCWSTR = w!("music-player-widget/spotify");

fn load_refresh_token() -> Option<String> {
    unsafe {
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        CredReadW(CRED_TARGET, CRED_TYPE_GENERIC, None, &mut cred).ok()?;
        let c = &*cred;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let token = String::from_utf8(bytes.to_vec()).ok();
        CredFree(cred as *const _);
        token
    }
}

fn store_refresh_token(token: &str) {
    let mut blob = token.as_bytes().to_vec();
    let mut user: Vec<u16> = "spotify".encode_utf16().chain(Some(0)).collect();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(w!("music-player-widget/spotify").as_ptr() as *mut u16),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = CredWriteW(&cred, 0);
    }
}

fn delete_refresh_token() {
    unsafe {
        let _ = CredDeleteW(CRED_TARGET, CRED_TYPE_GENERIC, None);
    }
}

// ---- PKCE helpers -------------------------------------------------------------------------

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    unsafe {
        let _ = BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
    }
    buf
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    unsafe {
        let _ = BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out);
    }
    out
}

fn base64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Value of `name` in a `a=b&c=d` query string.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

// ---- Worker --------------------------------------------------------------------------------

struct Auth {
    refresh: String,
    access: Option<(String, Instant)>,
}

fn post(window: isize, msg: u32, e: Event) {
    let ptr = Box::into_raw(Box::new(e));
    let sent = unsafe { PostMessageW(Some(HWND(window as *mut _)), msg, WPARAM(0), LPARAM(ptr as isize)) };
    if sent.is_err() {
        drop(unsafe { Box::from_raw(ptr) });
    }
}

/// Start the worker. `msg` is posted to `hwnd` with a `Box<Event>` pointer in `lparam`.
pub fn spawn(hwnd: HWND, msg: u32) {
    let window = hwnd.0 as isize;
    let (tx, rx) = mpsc::channel::<Job>();
    let _ = JOBS.set(tx);
    let _ = std::thread::Builder::new().stack_size(512 * 1024).spawn(move || {
        let mut auth = load_refresh_token().map(|refresh| Auth { refresh, access: None });
        post(window, msg, Event::Connected(auth.is_some()));
        while let Ok(job) = rx.recv() {
            match job {
                Job::Connect => match sign_in() {
                    Ok(a) => {
                        store_refresh_token(&a.refresh);
                        auth = Some(a);
                        post(window, msg, Event::Connected(true));
                    }
                    Err(notice) => post(window, msg, Event::Notice(notice)),
                },
                Job::Disconnect => {
                    delete_refresh_token();
                    auth = None;
                    post(window, msg, Event::Connected(false));
                }
                Job::Lookup { key } => {
                    let Some(a) = auth.as_mut() else { continue };
                    match lookup_now(a) {
                        Ok(Some((id, liked))) => post(window, msg, Event::Liked { key, id, liked }),
                        Ok(None) => post(window, msg, Event::Liked { key, id: String::new(), liked: None }),
                        Err(Failure::Unauthorized) => {
                            delete_refresh_token();
                            auth = None;
                            post(window, msg, Event::Connected(false));
                        }
                        Err(Failure::Other(notice)) => post(window, msg, Event::Notice(notice)),
                    }
                }
                Job::Set { key, id, liked } => {
                    let Some(a) = auth.as_mut() else { continue };
                    let ok = set_now(a, &id, liked).is_ok();
                    // On failure report the old state so the heart goes back.
                    let now = if ok { liked } else { !liked };
                    post(window, msg, Event::Liked { key, id, liked: Some(now) });
                    if !ok {
                        post(window, msg, Event::Notice("Couldn't update Liked Songs"));
                    }
                }
            }
        }
    });
}

enum Failure {
    /// The refresh token no longer works: sign in again.
    Unauthorized,
    Other(&'static str),
}

/// Exchange a code or refresh token for tokens.
fn token_request(form: String) -> Result<(String, Option<String>, i64), Failure> {
    let r = http::request(
        "POST",
        ACCOUNTS_HOST,
        "/api/token",
        "Content-Type: application/x-www-form-urlencoded\r\n",
        form.as_bytes(),
    )
    .map_err(|_| Failure::Other("Couldn't reach Spotify"))?;
    if r.status == 400 || r.status == 401 {
        return Err(Failure::Unauthorized);
    }
    if r.status != 200 {
        return Err(Failure::Other("Spotify sign-in failed"));
    }
    let access = http::json_string(&r.body, "access_token").ok_or(Failure::Other("Spotify sign-in failed"))?;
    let refresh = http::json_string(&r.body, "refresh_token");
    Ok((access, refresh, http::json_number(&r.body, "expires_in").unwrap_or(3600)))
}

/// Browser sign-in: PKCE, with a one-shot listener on 127.0.0.1:8888 for the redirect.
fn sign_in() -> Result<Auth, &'static str> {
    let client_id = client_id().ok_or("Add your client ID first")?;
    let verifier: String = random_bytes(64)
        .iter()
        .map(|b| b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"[(*b % 66) as usize] as char)
        .collect();
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let state = base64url(&random_bytes(12));

    let listener = TcpListener::bind("127.0.0.1:8888").map_err(|_| "Port 8888 is busy")?;
    listener.set_nonblocking(true).map_err(|_| "Sign-in failed")?;
    let url = format!(
        "https://{ACCOUNTS_HOST}/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&state={}&code_challenge_method=S256&code_challenge={}",
        http::encode(&client_id),
        http::encode(SCOPES),
        http::encode(REDIRECT_URI),
        state,
        challenge
    );
    open(&url);

    // Wait up to three minutes for the browser to come back.
    let deadline = Instant::now() + Duration::from_secs(180);
    let query = loop {
        if Instant::now() > deadline {
            return Err("Sign-in timed out");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = request.lines().next().unwrap_or_default().to_string();
                let query = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.strip_prefix("/callback?"))
                    .map(str::to_string);
                let page = if query.is_some() {
                    "You are connected. You can close this tab and go back to the widget."
                } else {
                    "Nothing to do here."
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>Music Player Widget</title><body style=\"font-family:sans-serif;padding:2em\">{page}</body>"
                );
                if let Some(q) = query {
                    break q;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return Err("Sign-in failed"),
        }
    };
    drop(listener);

    if query_param(&query, "state") != Some(state.as_str()) {
        return Err("Sign-in failed (state mismatch)");
    }
    let code = query_param(&query, "code").ok_or("Sign-in was cancelled")?;
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        code,
        http::encode(REDIRECT_URI),
        http::encode(&client_id),
        verifier
    );
    let (access, refresh, expires) = token_request(form).map_err(|f| match f {
        Failure::Unauthorized => "Spotify rejected the sign-in",
        Failure::Other(n) => n,
    })?;
    Ok(Auth {
        refresh: refresh.ok_or("Spotify sign-in failed")?,
        access: Some((access, Instant::now() + Duration::from_secs((expires - 60).max(30) as u64))),
    })
}

/// A valid access token, refreshing it when missing or about to expire.
fn access_token(auth: &mut Auth) -> Result<String, Failure> {
    if let Some((t, until)) = &auth.access {
        if Instant::now() < *until {
            return Ok(t.clone());
        }
    }
    let client_id = client_id().ok_or(Failure::Other("Add your client ID first"))?;
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        http::encode(&auth.refresh),
        http::encode(&client_id)
    );
    let (access, refresh, expires) = token_request(form)?;
    if let Some(r) = refresh {
        // Spotify may rotate the refresh token.
        if r != auth.refresh {
            store_refresh_token(&r);
            auth.refresh = r;
        }
    }
    auth.access = Some((access.clone(), Instant::now() + Duration::from_secs((expires - 60).max(30) as u64)));
    Ok(access)
}

fn api(auth: &mut Auth, method: &str, path: &str, body: &str) -> Result<http::Response, Failure> {
    let token = access_token(auth)?;
    let headers = format!("Authorization: Bearer {token}\r\nContent-Type: application/json\r\n");
    let r = http::request(method, API_HOST, path, &headers, body.as_bytes())
        .map_err(|_| Failure::Other("Couldn't reach Spotify"))?;
    if r.status == 401 {
        // Token revoked early: drop it and try once with a fresh one.
        auth.access = None;
        let token = access_token(auth)?;
        let headers = format!("Authorization: Bearer {token}\r\nContent-Type: application/json\r\n");
        return http::request(method, API_HOST, path, &headers, body.as_bytes())
            .map_err(|_| Failure::Other("Couldn't reach Spotify"));
    }
    Ok(r)
}

/// The track Spotify is playing now and whether it is in Liked Songs.
fn lookup_now(auth: &mut Auth) -> Result<Option<(String, Option<bool>)>, Failure> {
    let r = api(auth, "GET", "/v1/me/player/currently-playing", "")?;
    match r.status {
        200 => {}
        // Nothing playing, or a private session.
        204 => return Ok(None),
        401 => return Err(Failure::Unauthorized),
        403 => return Err(Failure::Other("Spotify refused access")),
        _ => return Err(Failure::Other("Couldn't read the playing track")),
    }
    let Some(at) = r.body.find("spotify:track:") else { return Ok(None) };
    let id: String = r.body[at + "spotify:track:".len()..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // The per-type /me/tracks endpoints were removed in February 2026; the library ones take URIs.
    let c = api(auth, "GET", &format!("/v1/me/library/contains?uris=spotify%3Atrack%3A{id}"), "")?;
    let liked = (c.status == 200).then(|| c.body.contains("true"));
    Ok(Some((id, liked)))
}

fn set_now(auth: &mut Auth, id: &str, liked: bool) -> Result<(), ()> {
    let method = if liked { "PUT" } else { "DELETE" };
    let r = api(auth, method, &format!("/v1/me/library?uris=spotify%3Atrack%3A{id}"), "").map_err(|_| ())?;
    (r.status / 100 == 2).then_some(()).ok_or(())
}
