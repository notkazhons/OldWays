# music-player-widget

Small native Windows widget that controls the Spotify desktop app. Goal: minimal RAM and CPU use.

## Decisions

- The widget does not play audio. Spotify keeps playing; the widget is a remote and display.
- Primary interface: Windows System Media Transport Controls (SMTC, `GlobalSystemMediaTransportControlsSessionManager`). No login, no network. Gives title, artist, album art, position, play state, play/pause/next/previous/seek.
- Optional later: Spotify Web API (Premium, OAuth PKCE) for volume, shuffle, repeat, like, queue. SMTC does not expose these.
- Native Win32 app, no webview, no Electron, no .NET runtime.
- Planned stack: Rust with the `windows` crate. Drawing via GDI or Direct2D.
- Idle target: redraw only on SMTC events; one timer for the progress bar only while playing and visible.

## Status

Scaffold only. Toolchain (Rust + MSVC build tools) not installed yet.
