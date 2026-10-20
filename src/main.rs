#![windows_subsystem = "windows"]

mod lyrics;
mod media;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{
    COLORREF, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmGetColorizationColor, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateBitmap, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, Ellipse, EndPaint, FW_NORMAL, FW_SEMIBOLD, FillRect, GetStockObject,
    HALFTONE, HBITMAP, HBRUSH, HDC, HFONT, InvalidateRect, NULL_BRUSH, NULL_PEN, OUT_DEFAULT_PRECIS,
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, PS_SOLID,
    Polygon, RoundRect, SRCCOPY, ScreenToClient, SelectObject, SetBkMode,
    SetStretchBltMode, SetTextColor, StretchBlt, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_LEFT,
    VK_RIGHT, VK_SPACE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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

/// Logical size in device-independent pixels at 96 DPI.
const WIDTH: i32 = 320;
const HEIGHT: i32 = 112;
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
/// Posted by a second launch to make the running instance show itself.
const WM_SHOW_REQUEST: u32 = WM_APP + 3;
/// Posted by the lyrics worker with a `Box<lyrics::Lyrics>` pointer in `lparam`.
const WM_LYRICS: u32 = WM_APP + 4;

/// Whether the window stays above all other windows. Off, it is an ordinary window that sits
/// behind apps but, being a tool window, stays visible after Win+D.
static TOPMOST: AtomicBool = AtomicBool::new(true);
static TIP: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn scale(hwnd: HWND, v: i32) -> i32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) } as i32;
    v * dpi.max(96) / 96
}

fn close_rect(hwnd: HWND) -> RECT {
    let (w, s) = (scale(hwnd, WIDTH), scale(hwnd, CLOSE_SIZE));
    RECT { left: w - s, top: 0, right: w, bottom: s }
}

/// Pin button, left of the close button. Bright ring = always on top, dim ring = normal window.
fn pin_rect(hwnd: HWND) -> RECT {
    let c = close_rect(hwnd);
    RECT { left: c.left - (c.right - c.left), top: c.top, right: c.left, bottom: c.bottom }
}

fn in_rect(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn state_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("music-player-widget");
    Some(dir.join("window.txt"))
}

/// Saved position, topmost flag and lyrics-box flag, if the file exists and the spot is still on screen.
fn load_state() -> Option<(i32, i32, bool, bool)> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let mut it = text.split_whitespace();
    let x: i32 = it.next()?.parse().ok()?;
    let y: i32 = it.next()?.parse().ok()?;
    // Older files wrote "1"/"0" or a mode name ("desktop" no longer exists and reads as off).
    let topmost = matches!(it.next()?, "1" | "top");
    let lyrics = it.next() == Some("lyrics");
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let visible = x >= vx - 40 && y >= vy && x < vx + vw - 40 && y < vy + vh - 40;
    visible.then_some((x, y, topmost, lyrics))
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
    let _ = std::fs::write(path, format!("{} {} {}{}", r.left, r.top, top, lyrics));
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

/// 32x32 green play triangle, built in memory so the exe needs no icon resource.
fn make_icon() -> Result<HICON> {
    const N: usize = 32;
    let mut px = [0u32; N * N];
    for y in 0..N {
        for x in 0..N {
            // Triangle with vertices (8,5), (8,26), (26,15.5).
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let half = (26.0 - fx) / 18.0 * 10.5;
            if (8.0..=26.0).contains(&fx) && (fy - 15.5).abs() <= half {
                px[y * N + x] = 0xFF1DB954;
            }
        }
    }
    unsafe {
        let color = CreateBitmap(N as i32, N as i32, 1, 32, Some(px.as_ptr() as *const _));
        let mask = CreateBitmap(N as i32, N as i32, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
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
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
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
            CMD_QUIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Prev,
    Toggle,
    Next,
    Seek,
    Lyrics,
    Pin,
    Close,
}

/// Right-hand column, left to right: art, then title / artist / progress row / controls.
const TEXT_X: i32 = PAD + ART + 12;
const CTRL_LEFT: i32 = 164;
const CTRL_TOP: i32 = 84;
const CTRL_W: i32 = 32;
const CTRL_H: i32 = 24;
const BAR_TOP: i32 = 64;
const BAR_H: i32 = 20;
/// Width reserved for each time label beside the progress bar.
const TIME_W: i32 = 34;

/// Clickable progress bar area, taller than the drawn track so it is easy to hit.
fn bar_rect(hwnd: HWND) -> RECT {
    RECT {
        left: scale(hwnd, TEXT_X + TIME_W + 4),
        top: scale(hwnd, BAR_TOP),
        right: scale(hwnd, WIDTH - 12 - TIME_W - 4),
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
    match b {
        Button::Prev => ctrl(0),
        Button::Toggle => ctrl(1),
        Button::Next => ctrl(2),
        Button::Seek => bar_rect(hwnd),
        // Right of the transport buttons, with a small gap.
        Button::Lyrics => RECT {
            left: scale(hwnd, CTRL_LEFT + 3 * CTRL_W + 10),
            top: scale(hwnd, CTRL_TOP),
            right: scale(hwnd, CTRL_LEFT + 4 * CTRL_W + 10),
            bottom: scale(hwnd, CTRL_TOP + CTRL_H),
        },
        Button::Pin => pin_rect(hwnd),
        Button::Close => close_rect(hwnd),
    }
}

fn button_at(hwnd: HWND, p: POINT) -> Option<Button> {
    [Button::Prev, Button::Toggle, Button::Next, Button::Seek, Button::Lyrics, Button::Pin, Button::Close]
        .into_iter()
        .find(|&b| in_rect(&button_rect(hwnd, b), p))
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
        Button::Pin => {
            set_topmost(hwnd, !TOPMOST.load(Ordering::Relaxed));
            save_state(hwnd);
        }
        Button::Toggle => media::command(media::Command::Toggle),
        Button::Next => media::command(media::Command::Next),
        Button::Prev => media::command(media::Command::Prev),
        // Seeking is committed on mouse release, not on click.
        Button::Seek => {}
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

fn window_height() -> i32 {
    HEIGHT + if UI.with_borrow(|ui| ui.lyrics_open) { LYRICS_H } else { 0 }
}

/// Resize to fit the lyrics box, keeping the top-left corner and staying on the work area.
fn resize(hwnd: HWND) {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (scale(hwnd, WIDTH), scale(hwnd, window_height()));
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

/// The once-a-second redraw runs only while a track plays and the window is visible.
fn sync_timer(hwnd: HWND, visible: bool) {
    UI.with_borrow_mut(|ui| {
        let want = visible && ui.playing && ui.dur_ms > 0;
        unsafe {
            if want && !ui.timer_on {
                SetTimer(Some(hwnd), TIMER_ID, 1000, None);
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
    lyrics_open: bool,
    lyrics: LyricsView,
    /// Track the lyrics (or the pending request) belong to.
    lyrics_key: String,
    /// First visible line of plain lyrics.
    lyric_scroll: i32,
    art: Option<HBITMAP>,
    playing: bool,
    can_prev: bool,
    can_next: bool,
    can_toggle: bool,
    can_seek: bool,
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
            lyrics_open: false,
            lyrics: LyricsView::Idle,
            lyrics_key: String::new(),
            lyric_scroll: 0,
            art: None,
            playing: false,
            can_prev: false,
            can_next: false,
            can_toggle: false,
            can_seek: false,
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
            Button::Lyrics | Button::Pin | Button::Close => true,
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
        self.drop_art();
        self.art_freed = new_art.is_some() && !visible;
        if visible {
            self.art = new_art.map(|a| unsafe {
                CreateBitmap(media::ART_SIZE, media::ART_SIZE, 1, 32, Some(a.bgra.as_ptr() as *const _))
            });
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

/// Play/pause, next and previous glyphs, drawn from polygons and bars.
fn draw_icon(hwnd: HWND, hdc: HDC, b: Button, rect: &RECT, color: COLORREF, playing: bool) {
    let (cx, cy) = ((rect.left + rect.right) as f32 / 2.0, (rect.top + rect.bottom) as f32 / 2.0);
    let u = scale(hwnd, 7) as f32;
    unsafe {
        let brush = CreateSolidBrush(color);
        let old_brush = SelectObject(hdc, brush.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        match b {
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
            Button::Seek | Button::Pin | Button::Close => {}
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
        let right = s(WIDTH - 12);
        SelectObject(hdc, title_font.into());
        SetTextColor(hdc, COLORREF(pal.title));
        let mut r = RECT { left, top: s(26), right, bottom: s(46) };
        draw_text(hdc, title, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        SelectObject(hdc, artist_font.into());
        SetTextColor(hdc, COLORREF(pal.artist));
        let mut r = RECT { left, top: s(46), right, bottom: s(64) };
        draw_text(hdc, artist, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);

        // Progress bar with elapsed and total time.
        if ui.dur_ms > 0 {
            let bar = button_rect(hwnd, Button::Seek);
            let can = ui.enabled(Button::Seek);
            let hot = can && (ui.hover == Some(Button::Seek) || ui.pressed == Some(Button::Seek));
            let (pos, dur) = (ui.position_ms(), ui.dur_ms);
            let width = bar.right - bar.left;
            let filled = (width as f64 * pos as f64 / dur as f64) as i32;
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
            let mut r = RECT { left: bar.right + s(4), top: bar.top, right: s(WIDTH - 8), bottom: bar.bottom };
            draw_text(hdc, &fmt_time(dur), &mut r, DT_LEFT | DT_VCENTER);
        }

        // Transport buttons.
        for b in [Button::Prev, Button::Toggle, Button::Next, Button::Lyrics] {
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
            } else if b == Button::Lyrics && ui.lyrics_open {
                accent
            } else if hot {
                pal.icon_hot
            } else {
                pal.icon
            };
            draw_icon(hwnd, hdc, b, &rect, COLORREF(color), ui.playing);
        }

        // Lyrics box below the controls.
        if ui.lyrics_open {
            let bx = RECT {
                left: s(PAD),
                top: s(HEIGHT),
                right: s(WIDTH - PAD),
                bottom: s(HEIGHT + LYRICS_H - PAD),
            };
            fill_round(hdc, bx.left, bx.top, bx.right, bx.bottom, s(10), pal.art_bg);
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
            LRESULT(if button_at(hwnd, p).is_some() { HTCLIENT } else { HTCAPTION } as isize)
        }
        WM_MOUSEMOVE => {
            let b = button_at(hwnd, client_point(lparam));
            let arm = UI.with_borrow_mut(|ui| {
                let mut changed = ui.hover != b;
                ui.hover = b;
                if ui.pressed == Some(Button::Seek) {
                    ui.scrub = Some(seek_ms_at(hwnd, client_point(lparam).x, ui.dur_ms));
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
            if let Some(b) = button_at(hwnd, client_point(lparam)) {
                if UI.with_borrow(|ui| ui.enabled(b)) {
                    UI.with_borrow_mut(|ui| {
                        ui.pressed = Some(b);
                        if b == Button::Seek {
                            ui.scrub = Some(seek_ms_at(hwnd, client_point(lparam).x, ui.dur_ms));
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
            let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            UI.with_borrow_mut(|ui| ui.update(*np, visible));
            sync_timer(hwnd, visible);
            if visible {
                ensure_lyrics();
            }
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
            if let Ok(icon) = make_icon() {
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
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: CreateSolidBrush(COLORREF(palette(read_light_theme()).bg)),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let saved = load_state();
        if let Some((_, _, top, lyrics)) = saved {
            TOPMOST.store(top, Ordering::Relaxed);
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
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let (w, h) = (scale(hwnd, WIDTH), scale(hwnd, window_height()));
        let (x, y) = saved.map_or_else(|| default_position(w, h), |(x, y, _, _)| (x, y));
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        create_tip(hwnd, instance.into())?;
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        add_tray(hwnd, make_icon()?);

        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            size_of_val(&round) as u32,
        );

        UI.with_borrow_mut(|ui| ui.light = read_light_theme());
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        media::spawn(hwnd, WM_NOWPLAYING);
        lyrics::spawn(hwnd, WM_LYRICS);

        // Blocks until a message arrives: no timers, no polling.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
