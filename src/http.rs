//! Small HTTPS client over WinHTTP, plus the one JSON helper the callers need.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{PCWSTR, w};

pub struct Response {
    pub status: u32,
    pub body: String,
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// One HTTPS request. `headers` is raw "Name: value\r\n" text. Blocks; call from a worker thread.
pub fn request(method: &str, host: &str, path: &str, headers: &str, body: &[u8]) -> Result<Response, ()> {
    unsafe {
        let session = Handle(WinHttpOpen(
            w!("OldWays/0.1 (https://github.com/notkazhons/OldWays)"),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(());
        }
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 10_000);
        let wide_host = wide(host);
        let connect = Handle(WinHttpConnect(session.0, PCWSTR(wide_host.as_ptr()), 443, 0));
        if connect.0.is_null() {
            return Err(());
        }
        let (wide_method, wide_path) = (wide(method), wide(path));
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(wide_method.as_ptr()),
            PCWSTR(wide_path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(());
        }
        let header_buf: Vec<u16> = headers.encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            if header_buf.is_empty() { None } else { Some(&header_buf) },
            if body.is_empty() { None } else { Some(body.as_ptr() as *const c_void) },
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .map_err(|_| ())?;
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

        let mut out = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|_| ())?;
            if available == 0 {
                break;
            }
            let mut buf = vec![0u8; available as usize];
            let mut read = 0u32;
            WinHttpReadData(request.0, buf.as_mut_ptr() as *mut c_void, available, &mut read).map_err(|_| ())?;
            out.extend_from_slice(&buf[..read as usize]);
            if out.len() > 2 * 1024 * 1024 {
                return Err(());
            }
        }
        Ok(Response { status, body: String::from_utf8_lossy(&out).into_owned() })
    }
}

/// Percent-encode everything except RFC 3986 unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Value of the first `"key": 123` number in a JSON document.
pub fn json_number(body: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\":");
    let after = body[body.find(&pat)? + pat.len()..].trim_start();
    let end = after.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Value of the first `"key": "..."` string in a JSON document; `None` for null or absent.
pub fn json_string(body: &str, key: &str) -> Option<String> {
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
