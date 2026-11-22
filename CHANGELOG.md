# Changelog

All notable changes to MacDock are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).
Each release on GitHub uses its section from this file as its notes.

## [Unreleased]

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

[Unreleased]: https://github.com/GodrezJr2/MacDock/compare/v0.4.2...HEAD
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
