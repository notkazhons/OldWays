#![windows_subsystem = "windows"]

mod media;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateBitmap, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, Ellipse, EndPaint, FW_NORMAL, FW_SEMIBOLD, FillRect, GetStockObject,
    HALFTONE, HBITMAP, HBRUSH, HDC, HFONT, InvalidateRect, NULL_BRUSH, NULL_PEN, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, PS_SOLID, Polygon, RoundRect, SRCCOPY, ScreenToClient, SelectObject, SetBkMode,
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
use windows::core::{PWSTR, Result, w};

/// Logical size in device-independent pixels at 96 DPI.
const WIDTH: i32 = 320;
const HEIGHT: i32 = 96;
const MARGIN: i32 = 16;
const CLOSE_SIZE: i32 = 28;
/// Album art square and the padding around it.
const ART: i32 = 80;
const PAD: i32 = 8;

/// Tray icon callback message and menu command ids.
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the media worker with a `Box<media::NowPlaying>` pointer in `lparam`.
const WM_NOWPLAYING: u32 = WM_APP + 2;
const CMD_TOGGLE: usize = 1;
const CMD_QUIT: usize = 2;

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

/// Saved position and topmost flag, if the file exists and the spot is still on screen.
fn load_state() -> Option<(i32, i32, bool)> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let mut it = text.split_whitespace();
    let x: i32 = it.next()?.parse().ok()?;
    let y: i32 = it.next()?.parse().ok()?;
    // Older files wrote "1"/"0" or a mode name ("desktop" no longer exists and reads as off).
    let topmost = matches!(it.next()?, "1" | "top");
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let visible = x >= vx - 40 && y >= vy && x < vx + vw - 40 && y < vy + vh - 40;
    visible.then_some((x, y, topmost))
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
    let _ = std::fs::write(path, format!("{} {} {}", r.left, r.top, top));
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

fn tray_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let label = if IsWindowVisible(hwnd).as_bool() { w!("Hide") } else { w!("Show") };
        let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE, label);
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit"));
        let p = cursor();
        // The menu only closes on an outside click if our window is foreground.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            CMD_TOGGLE => toggle_visible(hwnd),
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
    Pin,
    Close,
}

const CTRL_LEFT: i32 = 96;
const CTRL_TOP: i32 = 66;
const CTRL_W: i32 = 32;
const CTRL_H: i32 = 24;

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
        Button::Pin => pin_rect(hwnd),
        Button::Close => close_rect(hwnd),
    }
}

fn button_at(hwnd: HWND, p: POINT) -> Option<Button> {
    [Button::Prev, Button::Toggle, Button::Next, Button::Pin, Button::Close]
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
    }
}

/// What the window shows. Lives on the UI thread; the worker only posts updates.
struct Ui {
    active: bool,
    title: String,
    artist: String,
    art: Option<HBITMAP>,
    playing: bool,
    can_prev: bool,
    can_next: bool,
    can_toggle: bool,
    hover: Option<Button>,
    pressed: Option<Button>,
    /// A WM_MOUSELEAVE request is outstanding.
    tracking: bool,
    /// DPI the fonts were created for.
    dpi: u32,
    title_font: Option<HFONT>,
    artist_font: Option<HFONT>,
}

thread_local! {
    static UI: RefCell<Ui> = const {
        RefCell::new(Ui {
            active: false,
            title: String::new(),
            artist: String::new(),
            art: None,
            playing: false,
            can_prev: false,
            can_next: false,
            can_toggle: false,
            hover: None,
            pressed: None,
            tracking: false,
            dpi: 0,
            title_font: None,
            artist_font: None,
        })
    };
}

impl Ui {
    fn enabled(&self, b: Button) -> bool {
        match b {
            Button::Prev => self.can_prev,
            Button::Next => self.can_next,
            Button::Toggle => self.can_toggle,
            Button::Pin | Button::Close => true,
        }
    }

    fn update(&mut self, np: media::NowPlaying) {
        self.active = np.active;
        self.title = np.title;
        self.artist = np.artist;
        self.playing = np.playing;
        self.can_prev = np.can_prev;
        self.can_next = np.can_next;
        self.can_toggle = np.can_toggle;
        // Same track (e.g. just paused): keep the bitmap we already have.
        let new_art = match np.art {
            media::ArtUpdate::Keep => return,
            media::ArtUpdate::Set(a) => Some(a),
            media::ArtUpdate::Clear => None,
        };
        if let Some(old) = self.art.take() {
            unsafe {
                let _ = DeleteObject(old.into());
            }
        }
        self.art = new_art.map(|a| unsafe {
            CreateBitmap(media::ART_SIZE, media::ART_SIZE, 1, 32, Some(a.bgra.as_ptr() as *const _))
        });
    }

    fn free_fonts(&mut self) {
        for f in [self.title_font.take(), self.artist_font.take()].into_iter().flatten() {
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
    fn fonts(&mut self, hwnd: HWND) -> (HFONT, HFONT) {
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
        }
        (self.title_font.unwrap(), self.artist_font.unwrap())
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
            Button::Pin | Button::Close => {}
        }
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
        let (title_font, artist_font) = ui.fonts(hwnd);
        let s = |v| scale(hwnd, v);

        let bg = CreateSolidBrush(COLORREF(0x00201C1A));
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
                let b = CreateSolidBrush(COLORREF(0x002E2A28));
                FillRect(hdc, &art, b);
                let _ = DeleteObject(b.into());
                SelectObject(hdc, title_font.into());
                SetTextColor(hdc, COLORREF(0x00707070));
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
        let left = s(PAD + ART + 12);
        let right = s(WIDTH - 12);
        SelectObject(hdc, title_font.into());
        SetTextColor(hdc, COLORREF(0x00F0F0F0));
        let mut r = RECT { left, top: s(26), right, bottom: s(46) };
        draw_text(hdc, title, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        SelectObject(hdc, artist_font.into());
        SetTextColor(hdc, COLORREF(0x00A0A0A0));
        let mut r = RECT { left, top: s(46), right, bottom: s(64) };
        draw_text(hdc, artist, &mut r, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);

        // Transport buttons.
        for b in [Button::Prev, Button::Toggle, Button::Next] {
            let rect = button_rect(hwnd, b);
            let enabled = ui.enabled(b);
            let down = ui.pressed == Some(b);
            let hot = enabled && (down || ui.hover == Some(b));
            if hot {
                let bg = CreateSolidBrush(COLORREF(if down { 0x00504A46 } else { 0x00383330 }));
                let old_brush = SelectObject(hdc, bg.into());
                let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
                let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, s(8), s(8));
                SelectObject(hdc, old_pen);
                SelectObject(hdc, old_brush);
                let _ = DeleteObject(bg.into());
            }
            let color = if !enabled { 0x00505050 } else if hot { 0x00FFFFFF } else { 0x00D0D0D0 };
            draw_icon(hwnd, hdc, b, &rect, COLORREF(color), ui.playing);
        }

        // Buttons: close (x) and pin ring.
        SelectObject(hdc, title_font.into());
        SetTextColor(hdc, COLORREF(0x00D0D0D0));
        let mut r = close_rect(hwnd);
        draw_text(hdc, "\u{D7}", &mut r, DT_CENTER | DT_VCENTER);

        let on = TOPMOST.load(Ordering::Relaxed);
        let color = COLORREF(if on { 0x00D0D0D0 } else { 0x00707070 });
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
                let changed = ui.hover != b;
                ui.hover = b;
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
                    UI.with_borrow_mut(|ui| ui.pressed = Some(b));
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
                invalidate(hwnd);
                if button_at(hwnd, client_point(lparam)) == Some(pressed) {
                    activate(hwnd, pressed);
                }
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
            UI.with_borrow_mut(|ui| ui.update(*np));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
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
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let class = w!("MusicPlayerWidget");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: CreateSolidBrush(COLORREF(0x00201C1A)),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let saved = load_state();
        if let Some((_, _, top)) = saved {
            TOPMOST.store(top, Ordering::Relaxed);
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
        let (w, h) = (scale(hwnd, WIDTH), scale(hwnd, HEIGHT));
        let (x, y) = saved.map_or_else(|| default_position(w, h), |(x, y, _)| (x, y));
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

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        media::spawn(hwnd, WM_NOWPLAYING);

        // Blocks until a message arrives: no timers, no polling.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
