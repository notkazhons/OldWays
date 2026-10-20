# music-player-widget

Small native Windows music player widget. Goal: minimal RAM and CPU use.

## Decisions

- Source: local files. Spotify playback needs Premium, OAuth and a web API client, which adds RAM and complexity.
- Native Win32 app, no webview, no Electron, no .NET runtime.
- Planned stack: Rust with the `windows` crate. Audio via Media Foundation. Drawing via GDI or Direct2D.
- Idle target: window sleeps when nothing changes, no timers while paused.

## Status

Scaffold only. Toolchain (Rust + MSVC build tools) not installed yet.
