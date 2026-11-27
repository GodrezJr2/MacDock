# Changelog

All notable changes to MacDock are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).
Each release on GitHub uses its section from this file as its notes.

## [Unreleased]

## [0.5.2] - 2026-11-27

### Fixed
- A dock icon bounced whenever that app changed its window title in the background, as if it were asking for
  attention. A terminal with a spinner in its title (Claude Code, for one) bounced without end and kept the dock
  redrawing every frame. Only a real taskbar flash bounces now, and a request that keeps flashing bounces a few
  times and stops, as on macOS.

- Window fan cards looked soft and jagged: tilted cards now have smooth edges, and the window picture is
  shrunk by averaging its pixels instead of skipping them, so text and detail stay sharp.

### Changed
- The window fan and the menu bar panels (Control Center, Wi-Fi and the rest) animate once per screen refresh.
  They ran on a 64 Hz timer, which skipped a few frames a second on a 60 Hz screen and used under half the frames
  on a 144-165 Hz one.
- Lighter when idle: the dock no longer renders the whole desktop every 30 seconds to see if the wallpaper changed
  (it checks the wallpaper files first), AI usage re-reads only recent logs between its once-a-minute folder walk,
  and the menu bar reads the network state on its own thread instead of the one that draws the dock.

## [0.5.1] - 2026-11-27

### Added
- **Window fan**: rest the pointer on a dock app that has two or more windows and pictures of them fan out above
  its icon like a hand of cards on an arc. Hovering a card lifts it in front of the others; clicking it brings
  that window up (a minimized one comes back with the genie). Minimized windows show the picture saved when they
  went into the dock, or the app icon. Can be turned off in Settings (**Fan out windows on hover**, `WindowFan`).
- **Reset to Defaults** in Settings puts every setting of that window back to its default (click twice to
  confirm). Pinned apps, picked icons and the widget layout stay as they are.
- **Windows key opens Spotlight** (Settings › Desktop, `WinKeySpotlight`, off by default): a tap of the Windows key
  on its own opens Spotlight instead of Start. Win with another key (Win+E, Win+Shift+S, Win+L…) works as before,
  and Ctrl+Esc still opens Start. Tapping it again (or Esc) closes Spotlight, so a stray press is undone with
  one more tap.
- The Wi-Fi menu has a refresh button next to the switch, and shows a spinner there while it scans, also when you
  are already connected (it used to say "Searching…" only with no network joined). It scans again every 15 seconds
  while it stays open, and its text follows the interface language.

### Changed
- Each time the window fan opens it notes what each card was made from in `%APPDATA%\MacDock\fan.log` (kept under
  256 KB), to track down an empty card that sometimes shows up.

### Fixed
- Minimizing several windows of one app from the dock (a few Chrome windows, say) gives every one of them its genie
  again, and they come back with it too. Only the newest window's picture fitted in the snapshot memory, and Chrome
  draws covered windows black, so the others went into the dock and came back without the animation. Older
  pictures are now kept at half size instead of dropped, and a window that can't be pictured falls back to its
  earlier picture if it hasn't moved.

## [0.5.0] - 2026-11-26

### Added
- Desktop widgets can be arranged: each of the four slots shows a clock, a digital clock, the date, battery or system
  stats, or nothing (right-click a tile), and the widgets can be dragged anywhere, snapping to a screen corner when
  dropped near one. The layout is saved as `WidgetItems`, `WidgetPosition` and `WidgetOffset` in config.json.
  Clicks still open the tile's app; a drag starts only once the mouse has moved a few pixels, the last visible tile
  can't be hidden (so the right-click menu that brings tiles back stays reachable), and widgets put somewhere that a
  monitor or scale change has moved off-screen come back inside the work area. Thanks to @shekhaw. (#6)
- A **Digital Clock** tile, laid out like the date tile: the day, the time in large type and the date. (#6)
- A Settings button in Launchpad, a Settings result in app search and a **MacDock Settings…** item in the dock's
  menus all open MacDock's own Settings window, which "Edit config" used to open as raw config.json in Notepad. (#6)
- Updates install from the menu: **Update Available** downloads the new `macdock.exe`, checks its SHA-256 against the
  release's `macdock.exe.sha256`, swaps it in for the running exe (in whatever folder it is) and restarts MacDock.
  Before, the menu only opened the release page and the exe had to be replaced by hand.
  This works from 0.5.0 onward: updating to 0.5.0 itself is still done by hand. A download that breaks off is
  rejected rather than installed, a second click while one is running does nothing, and the old exe is kept as
  `macdock.exe.update-old` until the next start removes it.

### Fixed
- `config.json` is written to a temporary file and then swapped in, so a crash or power cut while saving can no
  longer leave a half-written file that the next start reads as no settings at all. The dock and the widgets each
  save only their own settings, so neither overwrites what the other just changed.
- The battery tile shows **AC Power** on a PC without a battery instead of a full ring.

## [0.4.5] - 2026-11-25

### Fixed
- Settings no longer runs off the bottom of short screens (1080p at 150% scale is 720 DIPs tall): the window stops above
  the dock and its content scrolls with the mouse wheel or the touchpad, edges fading where there is more. (#7)
- Moving the mouse over Settings no longer repaints the whole window on every move, only when the row under it
  changes, so the dock stays smooth beside it on integrated graphics. (#7)
- Quitting or restarting MacDock could leave its process spinning at 100% of a CPU core, impossible to end and
  blocking every later start until Windows restarted. MacDock now ends its process itself once its clean-up is done,
  and a new start only waits for a copy that is really still running.
- The dock's glass could look dark or show the wrong picture: it was painted from the wallpaper file, which is stale,
  missing or a different one with a slideshow, Windows Spotlight, a solid colour, a picture per monitor or a live
  wallpaper app. The glass (and the menu bar, widgets and Launchpad) now shows what the desktop really draws, on each
  monitor, and notices a slideshow or Spotlight change within 30 seconds.
- An invisible window behind the dock (an overlay faded all the way out, or one that draws nothing) no longer
  turns the glass black.

## [0.4.4] - 2026-11-25

### Added
- Docks on secondary monitors: every extra display gets its own dock, with its own reserved screen space and drag-and-drop.
  On by default; turn it off with **Docks on secondary monitors** in Settings (`SecondaryDocks` in config.json). (#5)

### Fixed
- Pinned shortcuts for different browser profiles (Brave, Chrome with `--profile-directory`) each show only their own
  profile's windows and running indicator, instead of all of them landing on the first pin. (#4)
- Alt+Tab no longer crashes MacDock when only one app is open and its name is long.
- MacDock only talks to the real Explorer taskbar when reserving screen space, never to another window that borrows its name.

## [0.4.3] - 2026-11-24

### Fixed
- Restore the dock and menu bar's reserved screen space after Windows Explorer restarts, so maximized windows
  no longer slide behind them.

## [0.4.2] - 2026-11-22

### Changed
- Genie: animation time follows the display's refresh clock (DWM vblank), so every frame steps evenly instead of
  juddering, with a finer mesh and a softer landing.
- Genie: snapshots are kept at full resolution (up to 2560 px wide), so a restore no longer turns from blurry to sharp
  at the end.
- Genie: when the dock minimizes or restores all of an app's windows, each window gets its own animation, cascading
  into (and out of) the icon with the front window on top. Before, only the front window animated and the rest
  vanished or popped in.
- Genie: windows have rounded corners and a soft drop shadow during the animation, the way Windows 11 draws them,
  so nothing pops at the start or the end.

### Fixed
- Genie: a restore no longer freezes on its last frame while the app handles the restore; the animation dissolves
  into the real window.
- Genie: windows restored together land in the same stacking order the animation shows them in.
- Alt+Tab: switching to an app whose windows the dock minimized together brings all of them back, not just one.

## [0.4.1] - 2026-11-22

### Fixed
- Alt+Tab: a quick second Alt+Tab sometimes opened the Windows switcher instead of MacDock's. The keyboard hook now
  runs on a thread of its own, so it always answers in time.

## [0.4.0] - 2026-11-22

### Added
- Apps window, as in macOS 26 (which replaced Launchpad with it): a Liquid Glass window with a search field that
  takes typing as soon as it opens, category chips (Utilities, Productivity & Finance, Entertainment, Social,
  Creativity, Developer Tools, Games, ...), a row of recent apps and every installed app. Arrow keys, Enter, Esc;
  right-click an app to keep it in the dock. ([#3])
- An Apps icon in the dock (after File Explorer) and in the menu bar opens it. The full-screen Launchpad is still
  there: Settings > All apps > Full screen. ([#3])
- Spotlight: the menu bar's magnifier, or Alt+Space, opens a glass capsule that grows into results as you type, apps
  first and a web search last. It replaces the Windows Search shortcut.
- Settings: a floating Mac-style window with traffic lights holding every option, animation speeds included, and the
  language (English or Indonesian). ([#2])

### Changed
- Menu bar panels fade and grow in and out; Control Center and the Launchpad open without waiting.
- Lighter: the full-screen Launchpad background is only built when that style is used (a fresh start measured about
  75 MB private on Windows 11).

### Fixed
- The Windows taskbar no longer flashes for 1-2 s after leaving fullscreen or minimizing. ([#1])
- Files with no registered app (for example .json on a stock Windows 10) open in Notepad instead of stalling about
  30 s.

## [0.3.1] - 2026-11-21

### Fixed
- Show Desktop (Win+D, or the three-finger swipe down on a touchpad) no longer hides the dock, the menu bar and the
  desktop widgets; before, only the wallpaper was left.
- Windows 10 (untested): Liquid Glass no longer captures the windows behind the dock where Windows cannot hide the
  yellow capture frame; the dock keeps its wallpaper glass there.

## [0.3.0] - 2026-11-21

### Added
- Alt+Tab opens a macOS-style app switcher: one icon per app in recent-use order on frosted glass. Tab, Shift+Tab and
  the arrows move, releasing Alt switches, Esc cancels, Q quits the app, H hides it; hover and click work too. A
  quick tap switches without showing the panel. Turn it off with `"AppSwitcher": false` in config.json.
- Drag files onto a dock icon to open them with that app (desktop apps get the files as arguments, Store apps
  through file activation). Drop them on the Trash to move them to the Recycle Bin.
- Releases are mirrored to Gitea.

### Changed
- Liquid Glass and the genie animation share one Direct3D device: about 30 MB less memory and 20 fewer threads after
  a minimize-all.

### Fixed
- The hover label hides while a dock menu is open.

## [0.2.0] - 2026-11-20

### Added
- Desktop widgets: four macOS-style widgets at the top right of the desktop (analog clock, today's date, battery
  ring, CPU / memory / disk bars) on frosted Liquid Glass made from the wallpaper. They sit under every window; a
  click opens Clock, Calendar, battery settings or Task Manager. Turn them off with `"DesktopWidgets": false`.
- Calendar on the clock: clicking the menu bar clock drops a month calendar (the arrows or the wheel change month,
  the title jumps back to today) with rows for Notifications and Date & Time settings.
- Update check: MacDock asks GitHub for the latest release at start and every 6 hours. A newer version puts a blue
  dot on the menu bar logo and an Update Available item in its menu that opens the release page; otherwise the menu
  offers Check for MacDock Updates. Nothing is downloaded or installed by itself.
- Menu bar switches: battery percentage, AI usage and tray icons can be turned on or off from the Windows-logo menu.

### Changed
- Liquid Glass dock: the dock plate shows the real windows behind it through a glass lens, and costs less CPU with
  a video playing behind it or while hovering at 144 Hz.

## [0.1.2] - 2026-11-14

### Fixed
- The Control Center brightness slider changes the built-in panel. It used to go through WMI, which was denied; it
  now uses the `\\.\LCD` brightness IOCTL.

## [0.1.1] - 2026-11-11

### Changed
- Releases are built by GitHub Actions.

### Fixed
- The genie animation releases its GPU buffers when it finishes. After minimizing a maximized window, memory used to
  stay at about 150 MB private (on an Intel iGPU, GPU memory is system RAM); it now drops back to about 70 MB.
- The dock and the menu bar hide for Chrome fullscreen started from a maximized window.
- Control Center's Power Mode shows the mode you picked.
- The dock restores minimized Administrator windows.

## [0.1.0] - 2026-11-10

First public release: a macOS-style dock (magnification, genie minimize and restore, auto-hide, unread badges,
attention bounce, recent apps and a Downloads stack) and menu bar (battery, Wi-Fi, Control Center, tray icons and
AI usage flyouts) for Windows 11, written in Rust.

[Unreleased]: https://github.com/GodrezJr2/MacDock/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/GodrezJr2/MacDock/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/GodrezJr2/MacDock/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/GodrezJr2/MacDock/compare/v0.4.5...v0.5.0
[0.4.5]: https://github.com/GodrezJr2/MacDock/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/GodrezJr2/MacDock/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/GodrezJr2/MacDock/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/GodrezJr2/MacDock/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/GodrezJr2/MacDock/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/GodrezJr2/MacDock/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/GodrezJr2/MacDock/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/GodrezJr2/MacDock/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/GodrezJr2/MacDock/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/GodrezJr2/MacDock/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/GodrezJr2/MacDock/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GodrezJr2/MacDock/releases/tag/v0.1.0
[#1]: https://github.com/GodrezJr2/MacDock/issues/1
[#2]: https://github.com/GodrezJr2/MacDock/issues/2
[#3]: https://github.com/GodrezJr2/MacDock/issues/3
