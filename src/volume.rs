//! Spotify's volume in the Windows volume mixer (its audio session), no login needed.
//! This is the app's mixer level, separate from the volume slider inside Spotify.

use windows::Win32::Media::Audio::{
    IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
    MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{Interface, Result};

/// Calls `f` for every audio session that belongs to Spotify; returns the last result.
fn for_spotify<R>(mut f: impl FnMut(&ISimpleAudioVolume) -> R) -> Result<Option<R>> {
    unsafe {
        // Ignore the result: S_FALSE (already initialised) and a mode mismatch are both fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        let mut last = None;
        for i in 0..sessions.GetCount()? {
            let control = sessions.GetSession(i)?;
            let control2: IAudioSessionControl2 = control.cast()?;
            let id = control2.GetSessionInstanceIdentifier()?.to_string().unwrap_or_default();
            if id.to_lowercase().contains("spotify.exe") {
                last = Some(f(&control.cast::<ISimpleAudioVolume>()?));
            }
        }
        Ok(last)
    }
}

/// (volume 0..=100, muted), or `None` when Spotify has no audio session (nothing playing yet).
pub fn get() -> Option<(i32, bool)> {
    for_spotify(|v| unsafe {
        let level = v.GetMasterVolume().unwrap_or(0.0);
        let muted = v.GetMute().map(|m| m.as_bool()).unwrap_or(false);
        ((level * 100.0).round() as i32, muted)
    })
    .ok()
    .flatten()
}

pub fn set(percent: i32) {
    let level = percent.clamp(0, 100) as f32 / 100.0;
    let _ = for_spotify(|v| unsafe {
        let _ = v.SetMasterVolume(level, std::ptr::null());
    });
}

pub fn set_mute(muted: bool) {
    let _ = for_spotify(|v| unsafe {
        let _ = v.SetMute(muted, std::ptr::null());
    });
}
