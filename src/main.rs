#![windows_subsystem = "windows"]

mod flow;
mod http;
mod lyrics;
mod media;
mod spotify;
mod volume;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{
    COLORREF, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmGetColorizationColor, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_OVER, AlphaBlend, BLENDFUNCTION, CreateRoundRectRgn, RestoreDC, SaveDC, SelectClipRgn,
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, DIB_RGB_COLORS, StretchDIBits, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateBitmap, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, Ellipse, EndPaint, FW_NORMAL, FW_SEMIBOLD, FillRect, GetStockObject,
    HALFTONE, HBITMAP, HBRUSH, HDC, HFONT, InvalidateRect, NULL_BRUSH, NULL_PEN, OUT_DEFAULT_PRECIS,
    GetMonitorInfoW, LineTo, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, MoveToEx,
    PAINTSTRUCT, PS_SOLID, Polygon, RoundRect, SRCCOPY, ScreenToClient, SelectObject, SetBkMode,
    SetStretchBltMode, SetTextColor, StretchBlt, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_LEFT,
    VK_RIGHT, VK_SPACE,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows::core::{BOOL, PCWSTR, PWSTR, Result, w};

/// Logical size in device-independent pixels at 96 DPI. The width is changed by dragging the
/// left or right edge and saved with the window position.
const DEFAULT_WIDTH: i32 = 360;
const MIN_WIDTH: i32 = 340;
const MAX_WIDTH: i32 = 900;
const HEIGHT: i32 = 112;
/// Width of the grab zone at each side edge.
const EDGE: i32 = 6;
/// Extra height while the lyrics box is open.
const LYRICS_H: i32 = 100;
const LYRIC_LINES: i32 = 4;
const LYRIC_LINE_H: i32 = 20;
const MARGIN: i32 = 16;
const CLOSE_SIZE: i32 = 28;
/// Album art square and the padding around it.
const ART: i32 = 96;
const PAD: i32 = 8;

/// Tray icon callback message and menu command ids.
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the media worker with a `Box<media::NowPlaying>` pointer in `lparam`.
const WM_NOWPLAYING: u32 = WM_APP + 2;
const CMD_TOGGLE: usize = 1;
const CMD_QUIT: usize = 2;
const CMD_AUTOSTART: usize = 3;
const CMD_SPOTIFY: usize = 4;
/// Posted by the Spotify Web API worker with a `Box<spotify::Event>` pointer in `lparam`.
const WM_SPOTIFY: u32 = WM_APP + 5;
/// Posted by a second launch to make the running instance show itself.
const WM_SHOW_REQUEST: u32 = WM_APP + 3;
/// Posted by the lyrics worker with a `Box<lyrics::Lyrics>` pointer in `lparam`.
const WM_LYRICS: u32 = WM_APP + 4;

/// Whether the window stays above all other windows. Off, it is an ordinary window that sits
/// behind apps but, being a tool window, stays visible after Win+D.
static TOPMOST: AtomicBool = AtomicBool::new(true);
static WIDTH_LP: AtomicI32 = AtomicI32::new(DEFAULT_WIDTH);
static TIP: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn width() -> i32 {
    WIDTH_LP.load(Ordering::Relaxed)
}

fn scale(hwnd: HWND, v: i32) -> i32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) } as i32;
    v * dpi.max(96) / 96
}

fn close_rect(hwnd: HWND) -> RECT {
    let (w, s) = (scale(hwnd, width()), scale(hwnd, CLOSE_SIZE));
    RECT { left: w - s, top: 0, right: w, bottom: s }
}

/// Pin button, left of the close button. Bright ring = always on top, dim ring = normal window.
fn pin_rect(hwnd: HWND) -> RECT {
    let c = close_rect(hwnd);
    RECT { left: c.left - (c.right - c.left), top: c.top, right: c.left, bottom: c.bottom }
}

/// Which side edge (true = left) the client point is on, for resizing.
fn edge_at(hwnd: HWND, p: POINT) -> Option<bool> {
    let (w, e) = (scale(hwnd, width()), scale(hwnd, EDGE));
    if p.x < e {
        Some(true)
    } else if p.x >= w - e {
        Some(false)
    } else {
        None
    }
}

/// Resize while an edge is dragged: the opposite edge (`anchor`, screen x) stays put.
fn drag_width(hwnd: HWND, left: bool, anchor: i32) {
    unsafe {
        let mut c = POINT::default();
        let _ = GetCursorPos(&mut c);
        let dpi = (GetDpiForWindow(hwnd) as i32).max(96);
        let phys = if left { anchor - c.x } else { c.x - anchor };
        let lw = (phys * 96 / dpi).clamp(MIN_WIDTH, MAX_WIDTH);
        if lw == width() {
            return;
        }
        WIDTH_LP.store(lw, Ordering::Relaxed);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let pw = scale(hwnd, lw);
        let x = if left { anchor - pw } else { r.left };
        let _ = SetWindowPos(hwnd, None, x, r.top, pw, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    update_tip(hwnd);
    invalidate(hwnd);
}

fn in_rect(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn state_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("music-player-widget");
    Some(dir.join("window.txt"))
}

/// Saved position, topmost flag, lyrics-box flag and width, if the file exists and the spot is still on screen.
fn load_state() -> Option<(i32, i32, bool, bool, i32)> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let mut it = text.split_whitespace();
    let x: i32 = it.next()?.parse().ok()?;
    let y: i32 = it.next()?.parse().ok()?;
    // Older files wrote "1"/"0" or a mode name ("desktop" no longer exists and reads as off).
    let topmost = matches!(it.next()?, "1" | "top");
    let (mut lyrics, mut w) = (false, DEFAULT_WIDTH);
    for tok in it {
        if tok == "lyrics" {
            lyrics = true;
        } else if let Some(n) = tok.strip_prefix('w').and_then(|n| n.parse::<i32>().ok()) {
            w = n.clamp(MIN_WIDTH, MAX_WIDTH);
        }
    }
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let visible = x >= vx - 40 && y >= vy && x < vx + vw - 40 && y < vy + vh - 40;
    visible.then_some((x, y, topmost, lyrics, w))
}

fn save_state(hwnd: HWND) {
    let Some(path) = state_path() else { return };
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let top = if TOPMOST.load(Ordering::Relaxed) { "top" } else { "normal" };
    let lyrics = if UI.with_borrow(|ui| ui.lyrics_open) { " lyrics" } else { "" };
    let _ = std::fs::write(path, format!("{} {} {}{} w{}", r.left, r.top, top, lyrics, width()));
}

fn default_position(w: i32, h: i32) -> (i32, i32) {
    let mut work = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    (work.right - w - MARGIN, work.bottom - h - MARGIN)
}

fn set_topmost(hwnd: HWND, on: bool) {
    TOPMOST.store(on, Ordering::Relaxed);
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    update_tip(hwnd);
}

fn tip_info(hwnd: HWND, text: &mut Vec<u16>) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd,
        uId: 1,
        rect: pin_rect(hwnd),
        lpszText: PWSTR(text.as_mut_ptr()),
        ..Default::default()
    }
}

fn tip_text() -> Vec<u16> {
    let s = if TOPMOST.load(Ordering::Relaxed) {
        "Above all windows (click: normal window)"
    } else {
        "Normal window, stays after Win+D (click: above all windows)"
    };
    s.encode_utf16().chain(Some(0)).collect()
}

fn update_tip(hwnd: HWND) {
    let tip = HWND(TIP.load(Ordering::Relaxed) as *mut _);
    if tip.0.is_null() {
        return;
    }
    let mut text = tip_text();
    let ti = tip_info(hwnd, &mut text);
    let lp = LPARAM(&ti as *const _ as isize);
    unsafe {
        SendMessageW(tip, TTM_UPDATETIPTEXTW, None, Some(lp));
        SendMessageW(tip, TTM_NEWTOOLRECTW, None, Some(lp));
    }
}

/// Native tooltip window, created once; the pin button is its only tool.
fn create_tip(hwnd: HWND, instance: HINSTANCE) -> Result<()> {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let tip = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASS,
            None,
            WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(hwnd),
            None,
            Some(instance),
            None,
        )?;
        TIP.store(tip.0 as isize, Ordering::Relaxed);
        let mut text = tip_text();
        let ti = tip_info(hwnd, &mut text);
        SendMessageW(tip, TTM_ADDTOOLW, None, Some(LPARAM(&ti as *const _ as isize)));
    }
    Ok(())
}

/// The cassette icon, embedded as a multi-size .ico whose images are PNG-compressed.
static ICON_ICO: &[u8] = include_bytes!("../assets/icon.ico");

/// Build an icon from the embedded .ico entry of `size` x `size` pixels.
fn make_icon(size: i32) -> Result<HICON> {
    let le16 = |o: usize| u16::from_le_bytes([ICON_ICO[o], ICON_ICO[o + 1]]) as usize;
    let le32 = |o: usize| u32::from_le_bytes(ICON_ICO[o..o + 4].try_into().unwrap()) as usize;
    let entry = (0..le16(4))
        .map(|i| 6 + i * 16)
        .find(|&e| ICON_ICO[e] as i32 == size % 256)
        .ok_or_else(windows::core::Error::empty)?;
    let (len, off) = (le32(entry + 8), le32(entry + 12));
    unsafe { CreateIconFromResourceEx(&ICON_ICO[off..off + len], true, 0x0003_0000, size, size, LR_DEFAULTCOLOR) }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn add_tray(hwnd: HWND, icon: HICON) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    for (dst, src) in nid.szTip.iter_mut().zip("Music Player Widget".encode_utf16()) {
        *dst = src;
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
}

fn toggle_visible(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, if IsWindowVisible(hwnd).as_bool() { SW_HIDE } else { SW_SHOWNOACTIVATE });
    }
}

fn cursor() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("MusicPlayerWidget");

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) }.is_ok()
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value: Vec<u16> = format!("\"{}\" --autostart", exe.display()).encode_utf16().chain(Some(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(value.as_ptr() as *const _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

/// Ask Windows to take unused pages out of our working set.
fn trim_memory() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Sign in to Spotify, or open the config file first when there is no client ID yet.
fn start_spotify_sign_in() {
    if spotify::client_id().is_some() {
        UI.with_borrow_mut(|ui| ui.notice = Some("Finish signing in in your browser"));
        spotify::connect();
    } else {
        spotify::open_config();
    }
}

/// Put `text` on the clipboard as Unicode text.
fn copy_text(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = (|| {
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let dst = GlobalLock(mem) as *mut u16;
            if dst.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(mem);
            // The clipboard owns the memory once this succeeds.
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).ok()
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

fn tray_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let label = if IsWindowVisible(hwnd).as_bool() { w!("Hide") } else { w!("Show") };
        let auto = autostart_enabled();
        let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE, label);
        let _ = AppendMenuW(
            menu,
            if auto { MF_STRING | MF_CHECKED } else { MF_STRING },
            CMD_AUTOSTART,
            w!("Start with Windows"),
        );
        let connected = UI.with_borrow(|ui| ui.spotify_connected);
        let spotify_label = if connected { w!("Disconnect Spotify") } else { w!("Connect Spotify (like button)") };
        let _ = AppendMenuW(menu, MF_STRING, CMD_SPOTIFY, spotify_label);
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit"));
        let p = cursor();
        // The menu only closes on an outside click if our window is foreground.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            CMD_TOGGLE => toggle_visible(hwnd),
            CMD_AUTOSTART => set_autostart(!auto),
            CMD_SPOTIFY => {
                if connected {
                    spotify::disconnect();
                } else {
                    start_spotify_sign_in();
                }
            }
            CMD_QUIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Like,
    Copy,
    Shuffle,
    Prev,
    Toggle,
    Next,
    Repeat,
    Mute,
    Volume,
    Seek,
    Lyrics,
    Minimize,
    Pin,
    Close,
}

/// Right-hand column, left to right: art, then title / artist / progress row / controls.
/// Controls row: shuffle, previous, play/pause, next, repeat, then speaker and volume slider.
const TEXT_X: i32 = PAD + ART + 12;
const CTRL_LEFT: i32 = TEXT_X;
const CTRL_TOP: i32 = 84;
const CTRL_W: i32 = 28;
const CTRL_H: i32 = 24;
const LIKE_W: i32 = 24;
const BAR_TOP: i32 = 64;
const BAR_H: i32 = 20;
/// Width reserved for each time label beside the progress bar.
const TIME_W: i32 = 34;

/// Clickable progress bar area, taller than the drawn track so it is easy to hit.
fn bar_rect(hwnd: HWND) -> RECT {
    RECT {
        left: scale(hwnd, TEXT_X + TIME_W + 4),
        top: scale(hwnd, BAR_TOP),
        right: scale(hwnd, width() - 12 - TIME_W - 4),
        bottom: scale(hwnd, BAR_TOP + BAR_H),
    }
}

/// Position in ms under client x, clamped to the bar.
fn seek_ms_at(hwnd: HWND, x: i32, dur_ms: i64) -> i64 {
    let r = bar_rect(hwnd);
    let frac = ((x - r.left) as f64 / (r.right - r.left).max(1) as f64).clamp(0.0, 1.0);
    (frac * dur_ms as f64) as i64
}

fn fmt_time(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn button_rect(hwnd: HWND, b: Button) -> RECT {
    let ctrl = |i: i32| RECT {
        left: scale(hwnd, CTRL_LEFT + i * CTRL_W),
        top: scale(hwnd, CTRL_TOP),
        right: scale(hwnd, CTRL_LEFT + (i + 1) * CTRL_W),
        bottom: scale(hwnd, CTRL_TOP + CTRL_H),
    };
    let row = |l: i32, r: i32| RECT {
        left: scale(hwnd, l),
        top: scale(hwnd, CTRL_TOP),
        right: scale(hwnd, r),
        bottom: scale(hwnd, CTRL_TOP + CTRL_H),
    };
    let row_at = |top: i32, l: i32, r: i32, h: i32| RECT {
        left: scale(hwnd, l),
        top: scale(hwnd, top),
        right: scale(hwnd, r),
        bottom: scale(hwnd, top + h),
    };
    let after_ctrls = CTRL_LEFT + 5 * CTRL_W + 8;
    match b {
        Button::Shuffle => ctrl(0),
        Button::Prev => ctrl(1),
        Button::Toggle => ctrl(2),
        Button::Next => ctrl(3),
        Button::Repeat => ctrl(4),
        Button::Mute => row(after_ctrls, after_ctrls + 24),
        Button::Volume => row(after_ctrls + 28, width() - 12),
        Button::Seek => bar_rect(hwnd),
        // Heart at the right end of the title row.
        Button::Like => row_at(26, width() - 12 - LIKE_W, width() - 12, 20),
        // Left of the heart, same row.
        Button::Copy => row_at(26, width() - 12 - 2 * LIKE_W, width() - 12 - LIKE_W, 20),
        // Top-right group, left of the pin: minimize, then lyrics.
        Button::Minimize | Button::Lyrics => {
            let p = pin_rect(hwnd);
            let w = p.right - p.left;
            let slot = if b == Button::Minimize { 1 } else { 2 };
            RECT { left: p.left - slot * w, top: p.top, right: p.left - (slot - 1) * w, bottom: p.bottom }
        }
        Button::Pin => pin_rect(hwnd),
        Button::Close => close_rect(hwnd),
    }
}

fn button_at(hwnd: HWND, p: POINT) -> Option<Button> {
    [
        Button::Shuffle,
        Button::Prev,
        Button::Toggle,
        Button::Next,
        Button::Repeat,
        Button::Mute,
        Button::Volume,
        Button::Seek,
        Button::Like,
        Button::Copy,
        Button::Lyrics,
        Button::Minimize,
        Button::Pin,
        Button::Close,
    ]
    .into_iter()
    .find(|&b| in_rect(&button_rect(hwnd, b), p))
}

/// Volume percent under client x on the slider.
fn volume_at(hwnd: HWND, x: i32) -> i32 {
    let r = button_rect(hwnd, Button::Volume);
    let frac = ((x - r.left) as f64 / (r.right - r.left).max(1) as f64).clamp(0.0, 1.0);
    (frac * 100.0).round() as i32
}

/// Move the volume to the slider position under `x`, unmuting, and tell the worker.
fn set_volume_from(hwnd: HWND, ui: &mut Ui, x: i32) {
    let v = volume_at(hwnd, x);
    if v != ui.volume || ui.muted {
        ui.volume = v;
        if ui.muted {
            ui.muted = false;
            media::command(media::Command::Mute(false));
        }
        media::command(media::Command::Volume(v));
    }
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn activate(hwnd: HWND, b: Button) {
    match b {
        Button::Close => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        // No taskbar entry, so minimize hides to the tray; the tray icon brings it back.
        Button::Copy => {
            let (url, ok) = UI.with_borrow(|ui| {
                // The exact track when the like lookup knows it, else a Spotify search for it.
                if ui.like_id.is_empty() {
                    let q = http::encode(&format!("{} {}", ui.title, ui.artist));
                    (format!("https://open.spotify.com/search/{q}"), false)
                } else {
                    (format!("https://open.spotify.com/track/{}", ui.like_id), true)
                }
            });
            let done = copy_text(hwnd, &url);
            UI.with_borrow_mut(|ui| {
                ui.notice = Some(match (done, ok) {
                    (false, _) => "Couldn't copy",
                    (true, true) => "Track link copied",
                    (true, false) => "Search link copied (connect Spotify for the exact track)",
                })
            });
            invalidate(hwnd);
        }
        Button::Minimize => unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        },
        Button::Pin => {
            set_topmost(hwnd, !TOPMOST.load(Ordering::Relaxed));
            save_state(hwnd);
        }
        Button::Toggle => media::command(media::Command::Toggle),
        Button::Next => media::command(media::Command::Next),
        Button::Prev => media::command(media::Command::Prev),
        // Seeking is committed on mouse release, not on click.
        Button::Seek => {}
        // The volume slider acts while dragging (see WM_MOUSEMOVE).
        Button::Volume => {}
        Button::Shuffle => {
            let on = UI.with_borrow(|ui| ui.shuffle);
            media::command(media::Command::Shuffle(!on));
        }
        Button::Repeat => {
            // Off, then repeat the list, then repeat the track.
            let mode = UI.with_borrow(|ui| ui.repeat);
            media::command(media::Command::Repeat(match mode {
                0 => 2,
                2 => 1,
                _ => 0,
            }));
        }
        Button::Mute => {
            let muted = UI.with_borrow_mut(|ui| {
                ui.muted = !ui.muted;
                ui.muted
            });
            media::command(media::Command::Mute(muted));
            invalidate(hwnd);
        }
        Button::Like => {
            let action = UI.with_borrow_mut(|ui| {
                ui.notice = None;
                if !ui.spotify_connected {
                    return None;
                }
                match (ui.like, ui.like_id.is_empty()) {
                    (Some(liked), false) => {
                        // Flip now; the worker confirms (or reverts) when Spotify answers.
                        ui.like = Some(!liked);
                        Some(Some((ui.like_key.clone(), ui.like_id.clone(), !liked)))
                    }
                    _ => {
                        // Unknown (lookup failed or still running): ask again.
                        ui.like_key.clear();
                        Some(None)
                    }
                }
            });
            match action {
                None => start_spotify_sign_in(),
                Some(Some((key, id, liked))) => spotify::set_liked(key, id, liked),
                Some(None) => ensure_like(),
            }
            invalidate(hwnd);
        }
        Button::Lyrics => {
            UI.with_borrow_mut(|ui| {
                ui.lyrics_open = !ui.lyrics_open;
                ui.lyric_scroll = 0;
                if !ui.lyrics_open {
                    // Closing frees the text.
                    ui.lyrics = LyricsView::Idle;
                    ui.lyrics_key.clear();
                }
            });
            resize(hwnd);
            ensure_lyrics();
            save_state(hwnd);
        }
    }
}

/// Look up whether the current track is liked, once per track, when signed in.
fn ensure_like() {
    let key = UI.with_borrow_mut(|ui| {
        let key = format!("{}\u{1}{}", ui.title, ui.artist);
        if !ui.spotify_connected || !ui.active || ui.title.is_empty() {
            ui.like = None;
            ui.like_id.clear();
            ui.like_key.clear();
            return None;
        }
        if ui.like_key == key {
            return None;
        }
        ui.like = None;
        ui.like_id.clear();
        ui.like_key = key.clone();
        Some(key)
    });
    if let Some(key) = key {
        spotify::lookup(key);
    }
}

fn window_height() -> i32 {
    HEIGHT + if UI.with_borrow(|ui| ui.lyrics_open) { LYRICS_H } else { 0 }
}

/// Resize to fit the lyrics box, keeping the top-left corner and staying on the work area.
fn resize(hwnd: HWND) {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (scale(hwnd, width()), scale(hwnd, window_height()));
        let mut y = r.top;
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            y = y.min(mi.rcWork.bottom - h).max(mi.rcWork.top);
        }
        let _ = SetWindowPos(hwnd, None, r.left, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    invalidate(hwnd);
}

/// Request lyrics for the current track if the box is open and we do not have them yet.
fn ensure_lyrics() {
    let req = UI.with_borrow_mut(|ui| {
        let key = format!("{}\u{1}{}", ui.title, ui.artist);
        if !ui.lyrics_open {
            return None;
        }
        if !ui.active || ui.title.is_empty() {
            ui.lyrics = LyricsView::Idle;
            ui.lyrics_key.clear();
            return None;
        }
        if ui.lyrics_key == key {
            return None;
        }
        ui.lyrics_key = key.clone();
        ui.lyrics = LyricsView::Message("Loading lyrics\u{2026}");
        ui.lyric_scroll = 0;
        Some(lyrics::Request {
            key,
            artist: ui.artist.clone(),
            title: ui.title.clone(),
            album: ui.album.clone(),
            duration_ms: ui.dur_ms,
        })
    });
    if let Some(r) = req {
        lyrics::request(r);
    }
}

const TIMER_ID: usize = 1;
/// Sent when the system accent color changes (not exposed by the bindings).
const WM_DWMCOLORIZATIONCOLORCHANGED: u32 = 0x0320;

/// Redraw interval for the flowing background and the progress bar.
const FRAME_MS: u32 = 50;

/// The redraw timer runs only while a track plays and the window is visible.
fn sync_timer(hwnd: HWND, visible: bool) {
    UI.with_borrow_mut(|ui| {
        let want = visible && ui.playing;
        unsafe {
            if want && !ui.timer_on {
                SetTimer(Some(hwnd), TIMER_ID, FRAME_MS, None);
                ui.timer_on = true;
            } else if !want && ui.timer_on {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                ui.timer_on = false;
            }
        }
    });
}

/// What the lyrics box shows for the current track.
enum LyricsView {
    Idle,
    Message(&'static str),
    Synced(Vec<(i64, String)>),
    Plain(Vec<String>),
}

/// What the window shows. Lives on the UI thread; the worker only posts updates.
struct Ui {
    active: bool,
    title: String,
    artist: String,
    album: String,
    /// Signed in to the Spotify Web API (for the like button).
    spotify_connected: bool,
    /// Whether the current track is in Liked Songs, once known.
    like: Option<bool>,
    like_id: String,
    /// Track the like state (or the pending lookup) belongs to.
    like_key: String,
    /// A short message shown in place of the artist line until the next click or track.
    notice: Option<&'static str>,
    lyrics_open: bool,
    lyrics: LyricsView,
    /// Track the lyrics (or the pending request) belong to.
    lyrics_key: String,
    /// First visible line of plain lyrics.
    lyric_scroll: i32,
    art: Option<HBITMAP>,
    /// The art's three main colors, for the flowing background.
    palette: Option<[(u8, u8, u8); 3]>,
    /// Colors currently drawn; eases toward `palette` on a track change.
    flow_cur: Option<[flow::Rgb; 3]>,
    /// Seconds of flow so far; only advances while playing.
    flow_t: f32,
    flow_last: Option<Instant>,
    flow_buf: Vec<u32>,
    /// Edge being dragged to resize: (left edge?, screen x of the opposite edge).
    edge_drag: Option<(bool, i32)>,
    playing: bool,
    can_prev: bool,
    can_next: bool,
    can_toggle: bool,
    can_seek: bool,
    can_shuffle: bool,
    can_repeat: bool,
    shuffle: bool,
    /// 0 off, 1 repeat the track, 2 repeat the list.
    repeat: u8,
    /// Spotify's mixer volume 0..=100, or -1 when it has no audio session.
    volume: i32,
    muted: bool,
    dur_ms: i64,
    /// Position reported at `base_at`; the shown position advances from there while playing.
    base_ms: i64,
    base_at: Option<Instant>,
    /// Position being dragged to, while the mouse is down on the progress bar.
    scrub: Option<i64>,
    timer_on: bool,
    /// The album art bitmap was dropped because the window is hidden; ask for it on show.
    art_freed: bool,
    /// Apps use the light theme.
    light: bool,
    /// Dominant vivid color of the current album art.
    accent: Option<(u8, u8, u8)>,
    hover: Option<Button>,
    pressed: Option<Button>,
    /// A WM_MOUSELEAVE request is outstanding.
    tracking: bool,
    /// DPI the fonts were created for.
    dpi: u32,
    title_font: Option<HFONT>,
    artist_font: Option<HFONT>,
    time_font: Option<HFONT>,
}

thread_local! {
    static UI: RefCell<Ui> = const {
        RefCell::new(Ui {
            active: false,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            spotify_connected: false,
            like: None,
            like_id: String::new(),
            like_key: String::new(),
            notice: None,
            lyrics_open: false,
            lyrics: LyricsView::Idle,
            lyrics_key: String::new(),
            lyric_scroll: 0,
            art: None,
            palette: None,
            flow_cur: None,
            flow_t: 0.0,
            flow_last: None,
            flow_buf: Vec::new(),
            edge_drag: None,
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
            base_ms: 0,
            base_at: None,
            scrub: None,
            timer_on: false,
            art_freed: false,
            light: false,
            accent: None,
            hover: None,
            pressed: None,
            tracking: false,
            dpi: 0,
            title_font: None,
            artist_font: None,
            time_font: None,
        })
    };
}

impl Ui {
    fn enabled(&self, b: Button) -> bool {
        match b {
            Button::Prev => self.can_prev,
            Button::Next => self.can_next,
            Button::Toggle => self.can_toggle,
            Button::Seek => self.can_seek && self.dur_ms > 0,
            Button::Shuffle => self.can_shuffle,
            Button::Repeat => self.can_repeat,
            Button::Mute | Button::Volume => self.volume >= 0,
            Button::Copy => self.active && !self.title.is_empty(),
            Button::Like | Button::Lyrics | Button::Minimize | Button::Pin | Button::Close => true,
        }
    }

    /// Current position: the dragged-to spot, else the last report plus time played since.
    fn position_ms(&self) -> i64 {
        if let Some(s) = self.scrub {
            return s;
        }
        let ahead = match (self.playing, self.base_at) {
            (true, Some(at)) => at.elapsed().as_millis() as i64,
            _ => 0,
        };
        (self.base_ms + ahead).clamp(0, self.dur_ms.max(0))
    }

    fn drop_art(&mut self) {
        if let Some(old) = self.art.take() {
            unsafe {
                let _ = DeleteObject(old.into());
            }
        }
    }

    /// While hidden, new art is not turned into a bitmap; it is requested again on show.
    fn update(&mut self, np: media::NowPlaying, visible: bool) {
        self.active = np.active;
        self.title = np.title;
        self.artist = np.artist;
        self.album = np.album;
        self.playing = np.playing;
        self.can_prev = np.can_prev;
        self.can_next = np.can_next;
        self.can_toggle = np.can_toggle;
        self.can_seek = np.can_seek;
        self.can_shuffle = np.can_shuffle;
        self.can_repeat = np.can_repeat;
        self.shuffle = np.shuffle;
        self.repeat = np.repeat;
        // Do not fight the slider while it is being dragged.
        if self.pressed != Some(Button::Volume) {
            self.volume = np.volume;
        }
        self.muted = np.muted;
        self.dur_ms = np.dur_ms;
        self.base_ms = np.pos_ms;
        self.base_at = Some(Instant::now());
        // Same track (e.g. just paused): keep the bitmap we already have.
        let new_art = match np.art {
            media::ArtUpdate::Keep => return,
            media::ArtUpdate::Set(a) => Some(a),
            media::ArtUpdate::Clear => None,
        };
        self.accent = new_art.as_ref().and_then(|a| a.accent);
        self.palette = new_art.as_ref().map(|a| a.palette);
        self.drop_art();
        self.art_freed = new_art.is_some() && !visible;
        if visible {
            self.art = new_art.map(|a| unsafe {
                CreateBitmap(media::ART_SIZE, media::ART_SIZE, 1, 32, Some(a.bgra.as_ptr() as *const _))
            });
        }
    }

    /// Flowing color behind everything, tinted by the art. Frozen while paused.
    fn draw_flow(&mut self, hdc: HDC, w: i32, h: i32, pal: &Palette) {
        let now = Instant::now();
        let dt = self.flow_last.map_or(0.0, |t| now.duration_since(t).as_secs_f32()).min(0.2);
        self.flow_last = Some(now);
        if self.playing {
            self.flow_t += dt;
        }
        let rgb = |c: (u8, u8, u8)| [c.0 as f32, c.1 as f32, c.2 as f32];
        let target: [flow::Rgb; 3] = match self.palette {
            Some(p) => p.map(rgb),
            None => {
                // No art: shades of the accent.
                let a = rgb(self.accent.unwrap_or_else(system_accent));
                [a, a.map(|v| v * 0.6), a.map(|v| v + (255.0 - v) * 0.3)]
            }
        };
        // Ease between tracks while animating; snap when nothing is running.
        let cur = match self.flow_cur {
            Some(c) if self.timer_on => {
                std::array::from_fn(|i| std::array::from_fn(|k| c[i][k] + (target[i][k] - c[i][k]) * 0.1))
            }
            _ => target,
        };
        self.flow_cur = Some(cur);
        let base = [(pal.bg & 0xFF) as f32, ((pal.bg >> 8) & 0xFF) as f32, ((pal.bg >> 16) & 0xFF) as f32];
        // The buffer is half size and stretched up, which keeps a frame cheap.
        let (bw, bh) = ((w / 2).max(2), (h / 2).max(2));
        let strength = if self.light { 0.45 } else { 0.65 };
        flow::render(&mut self.flow_buf, bw as usize, bh as usize, self.flow_t, base, &cur, strength);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bw,
                biHeight: -bh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            StretchDIBits(
                hdc,
                0,
                0,
                w,
                h,
                0,
                0,
                bw,
                bh,
                Some(self.flow_buf.as_ptr() as *const _),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }

    fn free_fonts(&mut self) {
        for f in [self.title_font.take(), self.artist_font.take(), self.time_font.take()]
            .into_iter()
            .flatten()
        {
            unsafe {
                let _ = DeleteObject(f.into());
            }
        }
    }

    fn free(&mut self) {
        self.free_fonts();
        if let Some(b) = self.art.take() {
            unsafe {
                let _ = DeleteObject(b.into());
            }
        }
    }

    /// Fonts are rebuilt only when the DPI changes.
    fn fonts(&mut self, hwnd: HWND) -> (HFONT, HFONT, HFONT) {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi != self.dpi || self.title_font.is_none() {
            self.free_fonts();
            self.dpi = dpi;
            let make = |px: i32, weight: i32| unsafe {
                CreateFontW(
                    -scale(hwnd, px),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    0,
                    w!("Segoe UI"),
                )
            };
            self.title_font = Some(make(15, FW_SEMIBOLD.0 as i32));
            self.artist_font = Some(make(13, FW_NORMAL.0 as i32));
            self.time_font = Some(make(11, FW_NORMAL.0 as i32));
        }
        (self.title_font.unwrap(), self.artist_font.unwrap(), self.time_font.unwrap())
    }
}

fn pt(x: f32, y: f32) -> POINT {
    POINT { x: x.round() as i32, y: y.round() as i32 }
}

fn fill(hdc: HDC, brush: HBRUSH, l: f32, t: f32, r: f32, b: f32) {
    let rect = RECT { left: l.round() as i32, top: t.round() as i32, right: r.round() as i32, bottom: b.round() as i32 };
    unsafe {
        FillRect(hdc, &rect, brush);
    }
}

/// Button glyphs drawn from polygons, bars and strokes. `state` is: playing for the play/pause
/// button, the repeat mode (0 off, 1 track, 2 list), or muted for the speaker.
fn draw_icon(hwnd: HWND, hdc: HDC, b: Button, rect: &RECT, color: COLORREF, state: i32) {
    let playing = state != 0;
    let (cx, cy) = ((rect.left + rect.right) as f32 / 2.0, (rect.top + rect.bottom) as f32 / 2.0);
    let u = scale(hwnd, 7) as f32;
    // A polyline in the icon color, 2 logical px wide.
    let stroke = |pts: &[(f32, f32)]| unsafe {
        let pen = CreatePen(PS_SOLID, scale(hwnd, 2), color);
        let old = SelectObject(hdc, pen.into());
        let _ = MoveToEx(hdc, pts[0].0.round() as i32, pts[0].1.round() as i32, None);
        for p in &pts[1..] {
            let _ = LineTo(hdc, p.0.round() as i32, p.1.round() as i32);
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(pen.into());
    };
    unsafe {
        let brush = CreateSolidBrush(color);
        let old_brush = SelectObject(hdc, brush.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        match b {
            Button::Shuffle => {
                stroke(&[(cx - u, cy - u * 0.6), (cx - u * 0.2, cy - u * 0.6), (cx + u * 0.3, cy + u * 0.6), (cx + u * 0.7, cy + u * 0.6)]);
                stroke(&[(cx - u, cy + u * 0.6), (cx - u * 0.2, cy + u * 0.6), (cx + u * 0.3, cy - u * 0.6), (cx + u * 0.7, cy - u * 0.6)]);
                let _ = Polygon(hdc, &[pt(cx + u * 0.6, cy + u * 0.25), pt(cx + u * 0.6, cy + u * 0.95), pt(cx + u * 1.05, cy + u * 0.6)]);
                let _ = Polygon(hdc, &[pt(cx + u * 0.6, cy - u * 0.95), pt(cx + u * 0.6, cy - u * 0.25), pt(cx + u * 1.05, cy - u * 0.6)]);
            }
            Button::Repeat => {
                stroke(&[(cx + u * 0.9, cy - u * 0.1), (cx + u * 0.9, cy - u * 0.7), (cx - u * 0.6, cy - u * 0.7), (cx - u * 0.6, cy - u * 0.2)]);
                stroke(&[(cx - u * 0.9, cy + u * 0.1), (cx - u * 0.9, cy + u * 0.7), (cx + u * 0.6, cy + u * 0.7), (cx + u * 0.6, cy + u * 0.2)]);
                let _ = Polygon(hdc, &[pt(cx - u * 0.95, cy - u * 0.25), pt(cx - u * 0.25, cy - u * 0.25), pt(cx - u * 0.6, cy + u * 0.3)]);
                let _ = Polygon(hdc, &[pt(cx + u * 0.95, cy + u * 0.25), pt(cx + u * 0.25, cy + u * 0.25), pt(cx + u * 0.6, cy - u * 0.3)]);
                if state == 1 {
                    // A small "1" in the middle marks repeat-one.
                    fill(hdc, brush, cx - u * 0.1, cy - u * 0.4, cx + u * 0.15, cy + u * 0.4);
                    fill(hdc, brush, cx - u * 0.35, cy - u * 0.4, cx - u * 0.1, cy - u * 0.2);
                }
            }
            Button::Mute => {
                fill(hdc, brush, cx - u, cy - u * 0.35, cx - u * 0.4, cy + u * 0.35);
                let _ = Polygon(
                    hdc,
                    &[pt(cx - u * 0.4, cy - u * 0.35), pt(cx + u * 0.2, cy - u * 0.9), pt(cx + u * 0.2, cy + u * 0.9), pt(cx - u * 0.4, cy + u * 0.35)],
                );
                if state != 0 {
                    stroke(&[(cx + u * 0.5, cy - u * 0.5), (cx + u * 1.1, cy + u * 0.5)]);
                    stroke(&[(cx + u * 0.5, cy + u * 0.5), (cx + u * 1.1, cy - u * 0.5)]);
                } else {
                    stroke(&[(cx + u * 0.55, cy - u * 0.5), (cx + u * 0.9, cy), (cx + u * 0.55, cy + u * 0.5)]);
                }
            }
            Button::Copy => {
                // Two overlapping sheets, the usual copy glyph.
                let (a, d) = (u * 0.85, u * 0.45);
                stroke(&[(cx - a + d, cy - a - d), (cx + a + d, cy - a - d), (cx + a + d, cy + a - d)]);
                stroke(&[
                    (cx - a - d, cy - a + d),
                    (cx + a - d, cy - a + d),
                    (cx + a - d, cy + a + d),
                    (cx - a - d, cy + a + d),
                    (cx - a - d, cy - a + d),
                ]);
            }
            Button::Like => {
                // Heart curve; filled when liked, outline otherwise.
                let k = u / 15.0;
                let pts: Vec<POINT> = (0..30)
                    .map(|i| {
                        let t = (i as f32 * 12.0).to_radians();
                        let x = 16.0 * t.sin().powi(3);
                        let y = -(13.0 * t.cos() - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos());
                        pt(cx + x * k, cy + y * k + u * 0.15)
                    })
                    .collect();
                if state != 0 {
                    let _ = Polygon(hdc, &pts);
                } else {
                    let pen = CreatePen(PS_SOLID, scale(hwnd, 2), color);
                    let (op, ob) = (SelectObject(hdc, pen.into()), SelectObject(hdc, GetStockObject(NULL_BRUSH)));
                    let _ = Polygon(hdc, &pts);
                    SelectObject(hdc, op);
                    SelectObject(hdc, ob);
                    let _ = DeleteObject(pen.into());
                }
            }
            Button::Toggle if playing => {
                let (bw, gap) = (u * 0.55, u * 0.45);
                fill(hdc, brush, cx - gap / 2.0 - bw, cy - u, cx - gap / 2.0, cy + u);
                fill(hdc, brush, cx + gap / 2.0, cy - u, cx + gap / 2.0 + bw, cy + u);
            }
            Button::Toggle => {
                let _ = Polygon(hdc, &[pt(cx - u * 0.7, cy - u), pt(cx - u * 0.7, cy + u), pt(cx + u * 0.9, cy)]);
            }
            Button::Next => {
                let _ = Polygon(
                    hdc,
                    &[pt(cx - u * 0.8, cy - u * 0.8), pt(cx - u * 0.8, cy + u * 0.8), pt(cx + u * 0.5, cy)],
                );
                fill(hdc, brush, cx + u * 0.6, cy - u * 0.8, cx + u * 0.95, cy + u * 0.8);
            }
            Button::Prev => {
                let _ = Polygon(
                    hdc,
                    &[pt(cx + u * 0.8, cy - u * 0.8), pt(cx + u * 0.8, cy + u * 0.8), pt(cx - u * 0.5, cy)],
                );
                fill(hdc, brush, cx - u * 0.95, cy - u * 0.8, cx - u * 0.6, cy + u * 0.8);
            }
            Button::Lyrics => {
                // Three text lines, the last one shorter.
                let h = (u * 0.22).max(2.0);
                for (i, half) in [(-1.0, 0.9), (0.0, 0.9), (1.0, 0.45)] {
                    let y = cy + i * u * 0.75;
                    fill(hdc, brush, cx - u * 0.9, y - h / 2.0, cx - u * 0.9 + u * 2.0 * half, y + h / 2.0);
                }
            }
            Button::Minimize => {
                // Thin centered dash, as light as the pin ring and the close cross.
                let h = (u * 0.16).max(1.5);
                fill(hdc, brush, cx - u * 0.6, cy - h / 2.0, cx + u * 0.6, cy + h / 2.0);
            }
            Button::Volume | Button::Seek | Button::Pin | Button::Close => {}
        }
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(brush.into());
    }
}

/// Colors as 0x00BBGGRR. The window follows the system "apps" light/dark setting.
struct Palette {
    bg: u32,
    art_bg: u32,
    art_note: u32,
    title: u32,
    artist: u32,
    time: u32,
    track: u32,
    thumb: u32,
    hot_bg: u32,
    pressed_bg: u32,
    icon: u32,
    icon_hot: u32,
    icon_off: u32,
    pin_on: u32,
    pin_off: u32,
}

const DARK: Palette = Palette {
    bg: 0x00201C1A,
    art_bg: 0x002E2A28,
    art_note: 0x00707070,
    title: 0x00F0F0F0,
    artist: 0x00A0A0A0,
    time: 0x00909090,
    track: 0x004A4540,
    thumb: 0x00FFFFFF,
    hot_bg: 0x00383330,
    pressed_bg: 0x00504A46,
    icon: 0x00D0D0D0,
    icon_hot: 0x00FFFFFF,
    icon_off: 0x00505050,
    pin_on: 0x00D0D0D0,
    pin_off: 0x00707070,
};

const LIGHT: Palette = Palette {
    bg: 0x00F4F4F4,
    art_bg: 0x00DCDCDC,
    art_note: 0x00909090,
    title: 0x00202020,
    artist: 0x005A5A5A,
    time: 0x00707070,
    track: 0x00D0D0D0,
    thumb: 0x00202020,
    hot_bg: 0x00E0E0E0,
    pressed_bg: 0x00CFCFCF,
    icon: 0x00303030,
    icon_hot: 0x00000000,
    icon_off: 0x00B0B0B0,
    pin_on: 0x00303030,
    pin_off: 0x00A0A0A0,
};

fn palette(light: bool) -> &'static Palette {
    if light { &LIGHT } else { &DARK }
}

fn read_light_theme() -> bool {
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && data != 0
}

/// System accent color, used when the album art gives none.
fn system_accent() -> (u8, u8, u8) {
    let (mut argb, mut opaque) = (0u32, BOOL(0));
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.is_ok() {
        ((argb >> 16) as u8, (argb >> 8) as u8, argb as u8)
    } else {
        (0x1D, 0xB9, 0x54)
    }
}

/// Nudge a color until it reads against the background: lighter on dark, darker on light.
fn readable(rgb: (u8, u8, u8), light: bool) -> u32 {
    let lum = |c: (f32, f32, f32)| 0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2;
    let mut c = (rgb.0 as f32, rgb.1 as f32, rgb.2 as f32);
    for _ in 0..8 {
        let l = lum(c);
        let (target, ok) = if light { (0.0, l < 150.0) } else { (255.0, l > 110.0) };
        if ok {
            break;
        }
        c = (c.0 + (target - c.0) * 0.15, c.1 + (target - c.1) * 0.15, c.2 + (target - c.2) * 0.15);
    }
    (c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16)
}

/// Rounded rectangle in a solid color, no outline.
fn fill_round(hdc: HDC, l: i32, t: i32, r: i32, b: i32, radius: i32, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let old_brush = SelectObject(hdc, brush.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = RoundRect(hdc, l, t, r, b, radius, radius);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(brush.into());
    }
}

/// Rounded box filled with `color` at `alpha` / 255, so what is behind it shows through.
fn fill_round_alpha(hdc: HDC, l: i32, t: i32, r: i32, b: i32, radius: i32, color: u32, alpha: u8) {
    unsafe {
        let saved = SaveDC(hdc);
        let rgn = CreateRoundRectRgn(l, t, r + 1, b + 1, radius, radius);
        SelectClipRgn(hdc, Some(rgn));
        // A 1x1 bitmap of the color, stretched over the box by AlphaBlend.
        let src = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, 1, 1);
        let old = SelectObject(src, bmp.into());
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(src, &RECT { left: 0, top: 0, right: 1, bottom: 1 }, brush);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 0 };
        let _ = AlphaBlend(hdc, l, t, r - l, b - t, src, 0, 0, 1, 1, blend);
        let _ = DeleteObject(brush.into());
        SelectObject(src, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(src);
        let _ = RestoreDC(hdc, saved);
        let _ = DeleteObject(rgn.into());
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn draw_text(hdc: HDC, s: &str, rect: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    // DrawTextW faults on a zero-length buffer (empty artist while Spotify shuts down).
    if s.is_empty() {
        return;
    }
    let mut buf = wide(s);
    unsafe {
        DrawTextW(hdc, &mut buf, rect, flags | DT_NOPREFIX | DT_SINGLELINE);
    }
}

fn draw(hwnd: HWND, hdc: HDC, w: i32, h: i32) {
    UI.with_borrow_mut(|ui| unsafe {
        let (title_font, artist_font, time_font) = ui.fonts(hwnd);
        let s = |v| scale(hwnd, v);
        let pal = palette(ui.light);
        let accent = readable(ui.accent.unwrap_or_else(system_accent), ui.light);

        let bg = CreateSolidBrush(COLORREF(pal.bg));
        FillRect(hdc, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
        let _ = DeleteObject(bg.into());
        ui.draw_flow(hdc, w, h, pal);
        SetBkMode(hdc, TRANSPARENT);

        // Album art, or a placeholder square.
        let art = RECT { left: s(PAD), top: s(PAD), right: s(PAD + ART), bottom: s(PAD + ART) };
        match ui.art {
            Some(bmp) => {
                let src = CreateCompatibleDC(Some(hdc));
                let old = SelectObject(src, bmp.into());
                SetStretchBltMode(hdc, HALFTONE);
                let _ = StretchBlt(
                    hdc,
                    art.left,
                    art.top,
                    art.right - art.left,
                    art.bottom - art.top,
                    Some(src),
                    0,
                    0,
                    media::ART_SIZE,
                    media::ART_SIZE,
                    SRCCOPY,
                );
                SelectObject(src, old);
                let _ = DeleteDC(src);
            }
            None => {
                let b = CreateSolidBrush(COLORREF(pal.art_bg));
                FillRect(hdc, &art, b);
                let _ = DeleteObject(b.into());
                SelectObject(hdc, title_font.into());
                SetTextColor(hdc, COLORREF(pal.art_note));
                let mut r = art;
                draw_text(hdc, "\u{266A}", &mut r, DT_CENTER | DT_VCENTER);
            }
        }

        // Title and artist.
        let (title, artist) = if !ui.active {
            ("Nothing playing", "Spotify is not running")
        } else if ui.title.is_empty() {
            ("Nothing playing", "")
        } else {
            (ui.title.as_str(), ui.artist.as_str())
        };
        let left = s(TEXT_X);
        let right = s(width() - 12);
        SelectObject(hdc, title_font.into());
        SetTextColor(hdc, COLORREF(pal.title));
        let mut r = RECT { left, top: s(26), right: right - s(2 * LIKE_W), bottom: s(46) };
        draw_text(hdc, title, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        SelectObject(hdc, artist_font.into());
        // A notice (sign-in progress, errors) replaces the artist line.
        let (artist, artist_color) = match ui.notice {
            Some(n) => (n, accent),
            None => (artist, pal.artist),
        };
        SetTextColor(hdc, COLORREF(artist_color));
        let mut r = RECT { left, top: s(46), right, bottom: s(64) };
        draw_text(hdc, artist, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);

        // Progress bar with elapsed and total time.
        if ui.dur_ms > 0 {
            let bar = button_rect(hwnd, Button::Seek);
            let can = ui.enabled(Button::Seek);
            let hot = can && (ui.hover == Some(Button::Seek) || ui.pressed == Some(Button::Seek));
            let (pos, dur) = (ui.position_ms(), ui.dur_ms);
            let bar_w = bar.right - bar.left;
            let filled = (bar_w as f64 * pos as f64 / dur as f64) as i32;
            let th = s(if hot { 6 } else { 4 });
            let cy = (bar.top + bar.bottom) / 2;
            let (y0, y1) = (cy - th / 2, cy - th / 2 + th);
            fill_round(hdc, bar.left, y0, bar.right, y1, th, pal.track);
            if filled > 0 {
                let color = if can { accent } else { pal.icon_off };
                fill_round(hdc, bar.left, y0, bar.left + filled.max(th), y1, th, color);
            }
            if hot {
                let d = s(12);
                let x = bar.left + filled;
                fill_round(hdc, x - d / 2, cy - d / 2, x + d / 2, cy + d / 2, d, pal.thumb);
            }

            SelectObject(hdc, time_font.into());
            SetTextColor(hdc, COLORREF(pal.time));
            let mut r = RECT { left: s(TEXT_X), top: bar.top, right: s(TEXT_X + TIME_W), bottom: bar.bottom };
            draw_text(hdc, &fmt_time(pos), &mut r, DT_RIGHT | DT_VCENTER);
            let mut r = RECT { left: bar.right + s(4), top: bar.top, right: s(width() - 8), bottom: bar.bottom };
            draw_text(hdc, &fmt_time(dur), &mut r, DT_LEFT | DT_VCENTER);
        }

        // Transport buttons.
        for b in [
            Button::Shuffle,
            Button::Prev,
            Button::Toggle,
            Button::Next,
            Button::Repeat,
            Button::Mute,
            Button::Like,
            Button::Copy,
            Button::Lyrics,
            Button::Minimize,
        ] {
            let rect = button_rect(hwnd, b);
            let enabled = ui.enabled(b);
            let down = ui.pressed == Some(b);
            let hot = enabled && (down || ui.hover == Some(b));
            if hot {
                let bg = CreateSolidBrush(COLORREF(if down { pal.pressed_bg } else { pal.hot_bg }));
                let old_brush = SelectObject(hdc, bg.into());
                let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
                let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, s(8), s(8));
                SelectObject(hdc, old_pen);
                SelectObject(hdc, old_brush);
                let _ = DeleteObject(bg.into());
            }
            let color = if !enabled {
                pal.icon_off
            } else if (b == Button::Lyrics && ui.lyrics_open)
                || (b == Button::Shuffle && ui.shuffle)
                || (b == Button::Repeat && ui.repeat != 0)
                || (b == Button::Like && ui.like == Some(true))
            {
                accent
            } else if b == Button::Like && !ui.spotify_connected && !hot {
                // Not signed in: dim until the user connects.
                pal.pin_off
            } else if hot {
                pal.icon_hot
            } else if matches!(b, Button::Shuffle | Button::Repeat) {
                // Off reads clearly dimmer than the other buttons.
                pal.pin_off
            } else {
                pal.icon
            };
            // The filled heart is its own cue; the others get a dot when on.
            let is_on = (b == Button::Lyrics && ui.lyrics_open)
                || (b == Button::Shuffle && ui.shuffle)
                || (b == Button::Repeat && ui.repeat != 0);
            if is_on {
                // Small dot under the icon, as a second cue besides the accent color.
                let (d, cx) = (s(4), (rect.left + rect.right) / 2);
                fill_round(hdc, cx - d / 2, rect.bottom - d - s(1), cx + d / 2, rect.bottom - s(1), d, accent);
            }
            let state = match b {
                Button::Toggle => ui.playing as i32,
                Button::Repeat => ui.repeat as i32,
                Button::Mute => ui.muted as i32,
                Button::Like => (ui.like == Some(true)) as i32,
                _ => 0,
            };
            draw_icon(hwnd, hdc, b, &rect, COLORREF(color), state);
        }

        // Volume slider.
        {
            let vr = button_rect(hwnd, Button::Volume);
            let enabled = ui.enabled(Button::Volume);
            let hot = enabled && (ui.hover == Some(Button::Volume) || ui.pressed == Some(Button::Volume));
            let level = if ui.muted { 0 } else { ui.volume.max(0) };
            let filled = ((vr.right - vr.left) as f64 * level as f64 / 100.0) as i32;
            let th = s(if hot { 6 } else { 4 });
            let cy = (vr.top + vr.bottom) / 2;
            let (y0, y1) = (cy - th / 2, cy - th / 2 + th);
            fill_round(hdc, vr.left, y0, vr.right, y1, th, pal.track);
            if filled > 0 {
                let color = if enabled { accent } else { pal.icon_off };
                fill_round(hdc, vr.left, y0, vr.left + filled.max(th), y1, th, color);
            }
            if hot {
                let d = s(12);
                let x = vr.left + filled;
                fill_round(hdc, x - d / 2, cy - d / 2, x + d / 2, cy + d / 2, d, pal.thumb);
            }
        }

        // Lyrics box below the controls.
        if ui.lyrics_open {
            let bx = RECT {
                left: s(PAD),
                top: s(HEIGHT),
                right: s(width() - PAD),
                bottom: s(HEIGHT + LYRICS_H - PAD),
            };
            // Tinted by the art instead of plain grey: a deep shade of the main color (dark theme) or a
            // pale wash of it (light theme), kept see-through.
            let tint = ui.flow_cur.map_or([0.0; 3], |c| c[0]);
            let (mix, white) = if ui.light { (0.12, 1.0) } else { (0.2, 0.0) };
            let ch = |v: f32| (white * (1.0 - mix) * 255.0 + v * mix).clamp(0.0, 255.0) as u32;
            let box_color = ch(tint[0]) | ch(tint[1]) << 8 | ch(tint[2]) << 16;
            fill_round_alpha(hdc, bx.left, bx.top, bx.right, bx.bottom, s(10), box_color, if ui.light { 140 } else { 150 });
            let line_h = s(LYRIC_LINE_H);
            let top = bx.top + (bx.bottom - bx.top - line_h * LYRIC_LINES) / 2;
            let (lx, rx) = (bx.left + s(10), bx.right - s(10));
            let line_rect = |row: i32| RECT { left: lx, top: top + row * line_h, right: rx, bottom: top + (row + 1) * line_h };
            match &ui.lyrics {
                LyricsView::Idle => {}
                LyricsView::Message(m) => {
                    SelectObject(hdc, artist_font.into());
                    SetTextColor(hdc, COLORREF(pal.artist));
                    let mut r = RECT { left: lx, top: bx.top, right: rx, bottom: bx.bottom };
                    draw_text(hdc, m, &mut r, DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS);
                }
                LyricsView::Synced(lines) => {
                    // Current line = last one that has started; one line of context above it.
                    let cur = lines.partition_point(|(t, _)| *t <= ui.position_ms()) as i32 - 1;
                    let start = cur.max(0) - 1;
                    for row in 0..LYRIC_LINES {
                        let Some((_, text)) = usize::try_from(start + row).ok().and_then(|i| lines.get(i)) else {
                            continue;
                        };
                        let current = start + row == cur;
                        SelectObject(hdc, if current { title_font } else { artist_font }.into());
                        SetTextColor(hdc, COLORREF(if current { accent } else { pal.artist }));
                        let mut r = line_rect(row);
                        let text = if text.is_empty() { "\u{266A}" } else { text.as_str() };
                        draw_text(hdc, text, &mut r, DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS);
                    }
                }
                LyricsView::Plain(lines) => {
                    let start = ui.lyric_scroll.clamp(0, (lines.len() as i32 - LYRIC_LINES).max(0));
                    SelectObject(hdc, artist_font.into());
                    SetTextColor(hdc, COLORREF(pal.title));
                    for row in 0..LYRIC_LINES {
                        if let Some(text) = lines.get((start + row) as usize) {
                            let mut r = line_rect(row);
                            draw_text(hdc, text, &mut r, DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS);
                        }
                    }
                }
            }
        }

        // Buttons: close (x) and pin ring.
        SelectObject(hdc, title_font.into());
        SetTextColor(hdc, COLORREF(pal.icon));
        let mut r = close_rect(hwnd);
        draw_text(hdc, "\u{D7}", &mut r, DT_CENTER | DT_VCENTER);

        let on = TOPMOST.load(Ordering::Relaxed);
        let color = COLORREF(if on { pal.pin_on } else { pal.pin_off });
        let pen = CreatePen(PS_SOLID, s(2), color);
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let (p, d) = (pin_rect(hwnd), s(10));
        let (cx, cy) = ((p.left + p.right) / 2, (p.top + p.bottom) / 2);
        let _ = Ellipse(hdc, cx - d / 2, cy - d / 2, cx + d / 2, cy + d / 2);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen.into());
    });
}

fn client_point(lparam: LPARAM) -> POINT {
    POINT { x: lparam.0 as i16 as i32, y: (lparam.0 >> 16) as i16 as i32 }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // Whole window drags like a title bar, except the buttons.
            let mut p = POINT { x: lparam.0 as i16 as i32, y: (lparam.0 >> 16) as i16 as i32 };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut p);
            }
            let client = button_at(hwnd, p).is_some() || edge_at(hwnd, p).is_some();
            LRESULT(if client { HTCLIENT } else { HTCAPTION } as isize)
        }
        WM_SETCURSOR => {
            if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                let mut p = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut p);
                    let _ = ScreenToClient(hwnd, &mut p);
                }
                let dragging = UI.with_borrow(|ui| ui.edge_drag.is_some());
                if dragging || (button_at(hwnd, p).is_none() && edge_at(hwnd, p).is_some()) {
                    unsafe {
                        SetCursor(LoadCursorW(None, IDC_SIZEWE).ok());
                    }
                    return LRESULT(1);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEMOVE => {
            if let Some((left, anchor)) = UI.with_borrow(|ui| ui.edge_drag) {
                drag_width(hwnd, left, anchor);
                return LRESULT(0);
            }
            let b = button_at(hwnd, client_point(lparam));
            let arm = UI.with_borrow_mut(|ui| {
                let mut changed = ui.hover != b;
                ui.hover = b;
                if ui.pressed == Some(Button::Seek) {
                    ui.scrub = Some(seek_ms_at(hwnd, client_point(lparam).x, ui.dur_ms));
                    changed = true;
                }
                if ui.pressed == Some(Button::Volume) {
                    set_volume_from(hwnd, ui, client_point(lparam).x);
                    changed = true;
                }
                let arm = !ui.tracking;
                ui.tracking = true;
                if changed {
                    invalidate(hwnd);
                }
                arm
            });
            if arm {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut t);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            UI.with_borrow_mut(|ui| {
                ui.hover = None;
                ui.tracking = false;
            });
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let pt = client_point(lparam);
            if button_at(hwnd, pt).is_none() {
                if let Some(left) = edge_at(hwnd, pt) {
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(hwnd, &mut r);
                        SetCapture(hwnd);
                    }
                    UI.with_borrow_mut(|ui| ui.edge_drag = Some((left, if left { r.right } else { r.left })));
                    return LRESULT(0);
                }
            }
            if let Some(b) = button_at(hwnd, client_point(lparam)) {
                if UI.with_borrow(|ui| ui.enabled(b)) {
                    UI.with_borrow_mut(|ui| {
                        ui.pressed = Some(b);
                        if b == Button::Seek {
                            ui.scrub = Some(seek_ms_at(hwnd, client_point(lparam).x, ui.dur_ms));
                        }
                        if b == Button::Volume {
                            set_volume_from(hwnd, ui, client_point(lparam).x);
                        }
                    });
                    unsafe {
                        SetCapture(hwnd);
                    }
                    invalidate(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if UI.with_borrow_mut(|ui| ui.edge_drag.take()).is_some() {
                unsafe {
                    let _ = ReleaseCapture();
                }
                save_state(hwnd);
                return LRESULT(0);
            }
            if let Some(pressed) = UI.with_borrow_mut(|ui| ui.pressed.take()) {
                unsafe {
                    let _ = ReleaseCapture();
                }
                if pressed == Button::Seek {
                    // Commit the drag: jump there now and tell Spotify.
                    let target = UI.with_borrow_mut(|ui| {
                        let t = ui.scrub.take();
                        if let Some(ms) = t {
                            ui.base_ms = ms;
                            ui.base_at = Some(Instant::now());
                        }
                        t
                    });
                    if let Some(ms) = target {
                        media::command(media::Command::Seek(ms));
                    }
                } else if button_at(hwnd, client_point(lparam)) == Some(pressed) {
                    activate(hwnd, pressed);
                }
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let b = match VIRTUAL_KEY(wparam.0 as u16) {
                VK_SPACE => Some(Button::Toggle),
                VK_LEFT => Some(Button::Prev),
                VK_RIGHT => Some(Button::Next),
                _ => None,
            };
            if let Some(b) = b {
                if UI.with_borrow(|ui| ui.enabled(b)) {
                    activate(hwnd, b);
                }
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            save_state(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            update_tip(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let (w, h) = (rc.right, rc.bottom);
                // Draw off-screen and copy once, so updates do not flicker.
                let mem = CreateCompatibleDC(Some(hdc));
                let bmp = CreateCompatibleBitmap(hdc, w, h);
                let old = SelectObject(mem, bmp.into());
                draw(hwnd, mem, w, h);
                let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(mem);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_NOWPLAYING => {
            // Ownership of the box passes from the worker thread to us.
            let np = unsafe { Box::from_raw(lparam.0 as *mut media::NowPlaying) };
            let mut visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            // Spotify just opened: bring the widget up, even if it was hidden to the tray.
            let opened = np.active && !UI.with_borrow(|ui| ui.active);
            if opened && !visible {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                visible = true;
            }
            UI.with_borrow_mut(|ui| ui.update(*np, visible));
            sync_timer(hwnd, visible);
            if visible {
                ensure_lyrics();
                ensure_like();
            }
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_SPOTIFY => {
            let e = unsafe { Box::from_raw(lparam.0 as *mut spotify::Event) };
            UI.with_borrow_mut(|ui| match *e {
                spotify::Event::Connected(on) => {
                    ui.spotify_connected = on;
                    ui.notice = None;
                    // Force a fresh lookup (or clear the heart) for the current track.
                    ui.like_key.clear();
                }
                spotify::Event::Liked { key, id, liked } => {
                    if key == ui.like_key {
                        ui.like_id = id;
                        ui.like = liked;
                    }
                }
                spotify::Event::Notice(n) => ui.notice = Some(n),
            });
            ensure_like();
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_LYRICS => {
            let l = unsafe { Box::from_raw(lparam.0 as *mut lyrics::Lyrics) };
            UI.with_borrow_mut(|ui| {
                // Ignore answers for a track we have moved on from, or a box that was closed.
                if ui.lyrics_open && ui.lyrics_key == l.key {
                    ui.lyrics = match l.body {
                        lyrics::Body::Synced(v) => LyricsView::Synced(v),
                        lyrics::Body::Plain(v) => LyricsView::Plain(v),
                        lyrics::Body::Message(m) => LyricsView::Message(m),
                    };
                }
            });
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // Plain (unsynced) lyrics scroll with the wheel; synced ones follow the song.
            let notches = (wparam.0 >> 16) as i16 as i32 / 120;
            UI.with_borrow_mut(|ui| {
                if ui.lyrics_open && matches!(ui.lyrics, LyricsView::Plain(_)) {
                    ui.lyric_scroll = (ui.lyric_scroll - notches).max(0);
                }
            });
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_SHOW_REQUEST => {
            // A second launch asks the running instance to come forward.
            unsafe {
                if !IsWindowVisible(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // lparam names the setting; "ImmersiveColorSet" is the light/dark/accent change.
            if lparam.0 != 0 {
                let name = unsafe { PCWSTR(lparam.0 as *const u16).to_string() }.unwrap_or_default();
                if name == "ImmersiveColorSet" {
                    UI.with_borrow_mut(|ui| ui.light = read_light_theme());
                    invalidate(hwnd);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DWMCOLORIZATIONCOLORCHANGED => {
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ID {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_SHOWWINDOW => {
            let visible = wparam.0 != 0;
            sync_timer(hwnd, visible);
            if !visible {
                // Hidden to the tray: drop the album art and give memory back to Windows.
                UI.with_borrow_mut(|ui| {
                    ui.art_freed = ui.art.is_some() || ui.art_freed;
                    ui.drop_art();
                    ui.flow_buf = Vec::new();
                    // Lyrics text goes too; it is fetched again on show if the box is open.
                    ui.lyrics = LyricsView::Idle;
                    ui.lyrics_key.clear();
                });
                trim_memory();
            } else {
                if UI.with_borrow_mut(|ui| std::mem::take(&mut ui.art_freed)) {
                    media::resend_art();
                }
                ensure_lyrics();
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => toggle_visible(hwnd),
                WM_RBUTTONUP => tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            save_state(hwnd);
            remove_tray(hwnd);
            UI.with_borrow_mut(|ui| ui.free());
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // Explorer restarted: its tray is empty again.
        _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            if let Ok(icon) = make_icon(16) {
                add_tray(hwnd, icon);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn main() -> Result<()> {
    unsafe {
        // One instance only: a second launch pokes the first one and exits.
        let _single = CreateMutexW(None, true, w!("Local\\MusicPlayerWidgetSingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(w!("MusicPlayerWidget"), None) {
                let _ = PostMessageW(Some(existing), WM_SHOW_REQUEST, WPARAM(0), LPARAM(0));
            }
            return Ok(());
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let class = w!("MusicPlayerWidget");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hIcon: make_icon(32).unwrap_or_default(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: CreateSolidBrush(COLORREF(palette(read_light_theme()).bg)),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let saved = load_state();
        if let Some((_, _, top, lyrics, w)) = saved {
            TOPMOST.store(top, Ordering::Relaxed);
            WIDTH_LP.store(w, Ordering::Relaxed);
            UI.with_borrow_mut(|ui| ui.lyrics_open = lyrics);
        }
        let ex = WS_EX_TOOLWINDOW
            | if TOPMOST.load(Ordering::Relaxed) { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };

        // Create at 96 DPI size, then resize once the window knows its monitor's DPI.
        let hwnd = CreateWindowExW(
            ex,
            class,
            w!("Music Player Widget"),
            WS_POPUP,
            0,
            0,
            width(),
            HEIGHT,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let (w, h) = (scale(hwnd, width()), scale(hwnd, window_height()));
        let (x, y) = saved.map_or_else(|| default_position(w, h), |(x, y, ..)| (x, y));
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        create_tip(hwnd, instance.into())?;
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        add_tray(hwnd, make_icon(16)?);

        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            size_of_val(&round) as u32,
        );

        UI.with_borrow_mut(|ui| ui.light = read_light_theme());
        // Started at login: stay in the tray until Spotify opens.
        if !std::env::args().any(|a| a == "--autostart") {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        media::spawn(hwnd, WM_NOWPLAYING);
        lyrics::spawn(hwnd, WM_LYRICS);
        spotify::spawn(hwnd, WM_SPOTIFY);

        // Blocks until a message arrives: no timers, no polling.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
