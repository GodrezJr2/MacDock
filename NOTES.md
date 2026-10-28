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

## Next ideas
- Minimize "genie" / scale effect into the dock, attention bounce for flashing windows.
- Drag files onto an icon to open with it; multi-monitor.
