# Roadmap

Each story has a goal and acceptance criteria. A story is done when every criterion is checked. Work in order; one story per commit or small group of commits. Update the status here when a story finishes.

Status: `todo`, `doing`, `done`.

Resource budget (checked in S10): private working set under 15 MB while playing, under 10 MB idle, 0% CPU when paused.

## S1: Toolchain and skeleton (done)
Goal: prove the Rust and MSVC toolchain builds a release binary.
- [x] `cargo build --release` succeeds.
- [x] Release profile tuned for size (`opt-level = "z"`, `lto`, `panic = "abort"`, `strip`).

## S2: Read current track (done)
Goal: read Spotify state through SMTC from a console binary, no UI yet.
- [x] Find the Spotify session in `GlobalSystemMediaTransportControlsSessionManager`.
- [x] Print title, artist, album, play state, position and duration.
- [x] Print a new line when the track or state changes (event driven, no polling loop).
- [x] Prints a clear message when Spotify is not running, and recovers when it starts.

## S3: Basic window (done)
Goal: a small borderless Win32 window with no content logic.
- [x] Fixed size, borderless, rounded corners, topmost option.
- [x] Draggable by clicking anywhere. Position saved and restored.
- [x] Close with a small button or Alt+F4. No taskbar entry option.
- [x] No timers running while idle.

## S3b: Pin button and tray (done)
Goal: control placement and visibility without a taskbar button.
- [x] Pin button toggles above all windows (bright ring) and normal window (dim ring). Hover tooltip names the state.
- [x] A normal window still stays visible after Win+D and stays behind apps (tested), so no desktop-layer attach is needed. Attaching as a child of the desktop view was tried and dropped: Windows rescaled its position under 125% display scaling and it drew nothing.
- [x] Tray icon: left click hides or shows, right click menu has Hide/Show and Quit. Icon is re-added after an Explorer restart.
- [x] Placement and mode are restored on launch.
## S4: Show now playing (done)
Goal: draw track info in the window.
- [x] Title and artist text, truncated with ellipsis when too long.
- [x] Album art loaded from the SMTC thumbnail, scaled to the window, freed on track change.
- [x] Updates on SMTC events only.
- [x] Empty state when Spotify is not running.

## S5: Transport controls (done)
Goal: control playback from the widget.
- [x] Play/pause, next, previous buttons with hover and pressed states.
- [x] Controls disabled when the action is not available.
- [x] Keyboard focus and basic shortcuts when the widget is focused (space, left, right).

## S6: Progress bar and seek (done)
Goal: show and change position.
- [x] Progress bar and elapsed/total time.
- [x] Redraw timer runs only while playing and the window is visible, at most 1 Hz.
- [x] Click or drag on the bar seeks.
- [x] Timer fully stops on pause, minimize or hide.

## S7: Look and feel (todo)
Goal: make it look good without costing memory.
- [ ] Dark and light theme follows the system setting.
- [ ] Accent color from album art or system accent.
- [ ] High DPI aware, crisp at 100% to 200% scaling.
- [ ] Smooth hover feedback without a constant animation timer.

## S8: Tray and behavior (todo)
Goal: stay out of the way.
- [x] Tray icon with show/hide and quit (done in S3b).
- [ ] Optional start with Windows.
- [ ] Single instance only.
- [ ] Hide to tray frees the album art bitmap.

## S9: Spotify Web API extras, optional (todo)
Goal: controls SMTC cannot do. Requires Premium and a one-time login.
- [ ] OAuth with PKCE, refresh token stored in Windows Credential Manager.
- [ ] Volume, shuffle, repeat, like.
- [ ] No extra network traffic or memory when the user has not logged in.

## S10: Efficiency pass and release (todo)
Goal: measure and lock in the budget.
- [ ] Measure working set and CPU idle, playing and paused. Record numbers in this file.
- [ ] Fix anything over budget.
- [ ] Release build under 1 MB if reasonable, zipped to GitHub Releases.
- [ ] README has install, usage and screenshots.
