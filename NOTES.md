# MacDock — notes

## Goal
A MyDockFinder-style macOS dock + menu bar for Jay's Windows 11 (Predator, 2560x1600 @125%), built from scratch
in .NET 10 WPF. Replaces the Windhawk taskbar restyle (ClearMacDock theme looked worse, reverted to LiquidGlass2).

## Run
- Build: `dotnet build -c Release` → `bin\Release\net10.0-windows\MacDock.exe`
- Quit: Windows logo (menu bar) → Quit MacDock. This also restores the taskbar.
- If it was killed and the taskbar is still hidden: `MacDock.exe --restore`
- Config: `%APPDATA%\MacDock\config.json` (Pinned list of `shell:AppsFolder\<AppID>` / .lnk / .exe,
  IconSize, Magnify, HideTaskbar, MenuBar). Errors: `%APPDATA%\MacDock\error.log`.

## What it does
- Dock: cosine magnification from the resting layout (R = 2.9 × icon), smoothed per frame; launch bounce
  (|sin|, 0.62 s period) until a new window shows up; running dots; hover label; Trash with separator;
  click = launch / activate / minimize if already in front; right-click menu (windows, Keep/Remove, Show in
  Explorer, Quit); drag pinned icons to reorder, drag up and out to remove.
- Menu bar: system menu, bold active app name, File/Edit/View/Window/Help that send real shortcuts to the app
  in front, battery, wifi, search, control center, clock.
- Both are AppBars (maximized windows stop at them), hide on fullscreen apps (ABN_FULLSCREENAPP).
- Hides the Windows taskbar (auto-hide + SW_HIDE), restores the original state on exit.

## Decisions / what failed
- Separate blur window behind the plate: SetWindowRgn is ignored for layered (AllowsTransparency) windows,
  so accent blur showed as a black square. A non-layered window with DWMWA_SYSTEMBACKDROP_TYPE also ignored
  the region (grey square, and it washed out the wallpaper colour). Dropped it.
- Instead the plate is painted with a blurred slice of the wallpaper (downsampled to 96 px wide,
  linear upscale), mapped to the plate's screen position. Works because the dock is an AppBar, so the wallpaper
  is almost always what's behind it. Reloads on wallpaper change.
- Acrylic accent made the plate milky; a light white gradient over the frost looks closer to macOS.
- WS_EX_NOACTIVATE keeps focus on the user's app, but WPF context menus close at once without focus:
  `Native.OpenMenu` lifts the flag while a menu is open.
- Screenshots from PowerShell need SetProcessDPIAware, otherwise the capture is scaled.

- Genie (Genie.cs): Windows' own minimize animation is switched off (SPI_SETANIMATION without
  SPIF_UPDATEINIFILE, so a reboot undoes it; restored on quit). Snapshots of the window in front are copied
  off the screen (BitBlt) on focus change, move end, every 1.5 s and right before a dock-click minimize. On
  EVENT_SYSTEM_MINIMIZESTART the snapshot is drawn as a 48-row mesh in a Viewport3D overlay and bent into
  the dock icon (squeeze sides, then slide rows down and pile them at the icon). Restore from the dock plays
  it backwards, then restores the window. Restores via Alt+Tab have no animation. `"Genie": false` turns it off.

## Performance (28 Oct 2026)
- Was 24% of a core idle and 640 MB: render loop ran at 165 Hz forever on a full-width layered window, and
  genie snapshots allocated full-size bitmaps on the UI thread every 1.5 s.
- Now: the dock loop sleeps once everything settles (values snap to their targets) and wakes on mouse,
  refresh changes or launches; the dock window is only as wide as a fully magnified dock; snapshots are
  shrunk by GDI StretchBlt into a DIB off the UI thread every 6 s; window AUMIDs are cached. Idle 2.5% of
  one core, ~230 MB private.
- The genie overlay is a non-layered full-screen window with its frame extended (DWM composes the alpha,
  no GPU readback): 125-165 fps vs choppy before. It is 1 px shorter than the screen, otherwise Explorer
  sends ABN_FULLSCREENAPP and the dock hid itself mid-animation; App.Fullscreen also checks the
  foreground window really covers the screen.

- Minimize "flash", round 2: the overlay is always shown full-screen and DWM-cloaked when idle (cloaked
  windows take no clicks, like other virtual desktops). Title-bar minimizes: a low-level mouse hook sees the
  press on a window's minimize button (WM_NCHITTEST = HTMINBUTTON), takes a fresh snapshot and draws it into
  the cloaked overlay; on release (same spot) it uncloaks over the window, so it is already up when the
  window vanishes. Without that (keyboard, PostMessage) there are ~2 frames of gap from the async
  EVENT_SYSTEM_MINIMIZESTART. DPI-unaware apps fail the hit test and fall back to that.
- Dead ends: parking the overlay at -32000 (it worked, but the "white frame" I blamed it for was a test
  click that maximized Notepad); WS_EX_LAYERED|TRANSPARENT added by hand (WPF strips LAYERED, the
  full-screen overlay then ate every click). Synthetic mouse_event clicks on caption buttons don't minimize,
  so real title-bar clicks can only be judged by Jay.
- `--restore` also puts Windows' minimize animation back (from WindowMetrics\MinAnimate), since a killed
  run leaves it off for the session.

## Next ideas
- Zoom-from-icon when an app opens its first window; attention bounce for flashing windows.
- Drag files onto an icon to open with it; multi-monitor.