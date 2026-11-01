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

## Game mode
- Fullscreen detection is polled on every dock refresh (foreground window covers the screen and is not the
  desktop), because Explorer's ABN_FULLSCREENAPP is unreliable with its taskbar hidden.
- While fullscreen: dock and menu bar hidden, render loop asleep, low-level mouse hook removed (it sat in the
  game's input path), snapshot captures stopped and dropped, refresh slowed to 3 s, then GC + working-set
  trim. Measured with Terminal in F11: 0.00% CPU, working set 17 MB; everything returns on exit.
- Snapshot pixels live in native WIC memory, so replaced ones piled up to 500+ MB private until a GC ran;
  Genie now collects every 6 snapshots.

## Restores without animation
- Snapshots used to live only in memory, so every MacDock restart (many on 28 Oct while iterating) left the
  already-minimized windows with nothing to animate. On each minimize the snapshot is now also written to
  %LOCALAPPDATA%\MacDock\snaps\<hwnd>_<pid>_<x>_<y>_<w>_<h>.png and reloaded at start for windows that are
  still minimized (pid must match, so a reused handle is ignored); the file goes when the window is restored.
- %APPDATA%\MacDock\genie.log records each minimize/restore that could not animate and why (no snapshot,
  no dock icon, overlay busy) with the process name. Read it when Jay reports a missing animation.

## Next ideas
- Zoom-from-icon when an app opens its first window; attention bounce for flashing windows.
- Drag files onto an icon to open with it; multi-monitor.
## 29 Oct 2026
- Dock restores came back behind other apps: the restore runs after the genie, when the mouse may have
  moved over another app and taken our SetForegroundWindow right. Shell.Activate sends an empty SendInput
  mouse event first (PowerToys trick), then AttachThreadInput, then a TOPMOST/NOTOPMOST lift.
- Genie on other monitors: the overlay spans the virtual screen in physical pixels (camera units = pixels,
  WM_DPICHANGED swallowed, refit after Show since WPF resizes it once). Monitors: primary 1920x1080 @100%,
  second 2560x1600 @125% at x=1920, y=-203.
- Icons on Tahoe squircle tiles (IconTile.cs, "IconTiles"). Jay kept the white tiles over the Sequoia look.
- Attention bounce: shell hook window, HSHELL_FLASH → hop-and-rest bounce until the app comes to front (max 5).
- Auto-hide ("AutoHide", menu bar system menu or right-click the plate): AppBar removed, content slides via
  a TranslateTransform; hidden it is fully transparent so clicks pass. The frame loop sleeps, so a 60 ms
  timer wakes it at the bottom edge and while the dock is up (otherwise it never noticed the pointer left).
- Jay wants every finished change committed right away.
- Next ideas: Recent apps section, Downloads stack, per-app icon overrides.

## Battery flyout (31 Oct 2026)
- Menu bar battery is now a drawn macOS-style glyph (BatteryGlyph.cs: fill by %, green while charging, bolt on AC).
  Left click opens BatteryPopup (no more Win+A / Wi-Fi panel); right click opens Windows' battery settings.
  Dev aid: `MacDock.exe --battery` shows the flyout alone.
- Flyout tiles: DC in / Total draw / From battery (Into battery while charging) + CPU/GPU/rest bar.
- Data (PowerMeter.cs): battery class IOCTLs give only battery-side rate (0 on AC when full or at the 80% limit);
  no adapter sensor exists (checked root\wmi, Acer WMI classes, EMI). Unprivileged sources that work: EMI v2 on the
  CPU PPM device (RAPL_Package0_PKG, picowatt-hours) and NVML GPU power. So on battery the total is measured
  (discharge rate); on AC total ≈ CPU + GPU + "rest", where rest is learned on battery (draw − CPU − GPU, EMA)
  and saved to %LOCALAPPDATA%\MacDock\power.json (default 14 W). DC in ≈ total + charge rate.
- Sensors are only polled while the flyout is open (NVML can wake a suspended dGPU), so calibration needs the
  flyout open on battery for ~a minute. Psys (MSR 0x64D) would be the real platform number but needs a kernel
  driver (OpenSense/PawnIO); not tried.
- Lag fix (1 Nov 2026): the flyout froze the pointer every second in iGPU / battery mode. Measured: nvmlInit 613 ms,
  nvmlDeviceGetPowerUsage ~470 ms per call while the dGPU sleeps (each query wakes it), all on the UI thread.
  Now PowerMeter.Read runs via Task.Run, NVML/EMI open lazily on the first read, a GPU read slower than 150 ms
  means "GPU off" (0 W, recheck every 30 s), and the flyout opens with the last cached sample. Jay confirmed no lag.

## Menu bar extras + fixes (31 Oct 2026, part 2)
- Flyout.cs: shared base for menu bar extras (dark panel, closes on Esc / focus loss, PlaceUnder). MenuBarWindow.Toggle
  opens one at a time; clicking the open item closes it.
- ^ item (TrayFlyout.cs + Tray.cs): hosts the notification area through ManagedShell 0.0.374 (RetroBar's library):
  it registers its own topmost Shell_TrayWnd, so Taskbar.cs now picks Explorer's Shell_TrayWnd by process.
  Clicks go to the app via NotifyIcon.IconMouseDown/Up (Discord opens, right click = app menu). `"TrayIcons": false`
  turns it off.
- Wi-Fi (WifiFlyout.cs): WinRT WiFiAdapter scan / connect (saved profile, else inline password) / disconnect, Radio
  on/off. Menu bar shows drawn Wi-Fi bars, or an Ethernet glyph on a wired connection. Right click = Wi-Fi settings.
- Control Center (ControlCenter.cs, SystemControls.cs): Wi-Fi + Bluetooth radios, Dark Mode (registry +
  WM_SETTINGCHANGE ImmersiveColorSet), Power Mode (PowerSetActiveOverlayScheme), brightness (WMI), volume (Core Audio
  IAudioEndpointVolume), Now Playing (GlobalSystemMediaTransportControls).
- TFM is now net10.0-windows10.0.22621.0 for WinRT; OutputPath pinned to bin\Release\net10.0-windows\.
- Search item still opens Windows search (Win+S); clock still opens the notification center.
- Discord showed twice: its pin resolves to the Squirrel stub …\Discord\Discord.exe, windows run from
  …\Discord\app-1.0.x\Discord.exe. DockItem.LauncherOf matches the same exe name (or Update.exe) below the pin's folder.
- Minimize did nothing in WinUI apps (OpenSense): the genie overlay was revealed on mouse-up, but those custom
  title bars don't hold mouse capture, so the up landed on our overlay. Early reveal now only when the window's
  thread holds the capture (GetGUIThreadInfo); otherwise the normal MINIMIZESTART path animates. Needs a real
  click by Jay to confirm.

### Tray hosting only while the ^ flyout is open (31 Oct 2026)
- Bug: with MacDock running, OpenSense (WinUI 3) crashed at startup. Log: `System.NotImplementedException: SetShownInSwitchers failed` in `H.NotifyIcon.TaskbarIcon.PrepareContextMenuWindow` (AppWindow.IsShownInSwitchers → E_NOTIMPL).
- Cause: ManagedShell's own topmost `Shell_TrayWnd`. Windows App SDK treats a non-Explorer Shell_TrayWnd as an unsupported shell. Turning `TrayIcons` off made OpenSense start fine.
- ManagedShell's ExplorerTrayService (reads Explorer's ToolbarWindow32) is no option: the Win11 tray is XAML.
- Fix: `Tray.Start()` runs when TrayFlyout opens and `Tray.Stop()` when it closes (a fresh ShellManager each time). Apps re-add their icons on TaskbarCreated within ~1 s, so the grid fills live and shows "Loading…" first. `Tray.Enabled` replaces the startup Start.
- Remaining risk: a WinUI app launched while the flyout is open can still hit the crash.
