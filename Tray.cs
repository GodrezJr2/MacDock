using System;
using System.Collections.ObjectModel;
using System.IO;
using ManagedShell;
using ManagedShell.WindowsTray;

namespace MacDock;

/// <summary>
/// The notification area (the icons behind Windows' ^ overflow: Discord, Steam, OneDrive…). With the taskbar
/// hidden they would be unreachable, so MacDock hosts them itself through ManagedShell (the library behind
/// RetroBar/Cairo): it registers a second Shell_TrayWnd and asks every app to re-add its icon (TaskbarCreated).
///
/// It does that only while the ^ flyout is open. A foreign Shell_TrayWnd on top makes Windows App SDK report the
/// shell as unsupported: AppWindow.IsShownInSwitchers throws E_NOTIMPL, and WinUI apps that set it at startup
/// (OpenSense through H.NotifyIcon) crash. Closing the flyout destroys the window and broadcasts TaskbarCreated
/// again, so every icon goes back to Explorer.
/// </summary>
static class Tray
{
    static ShellManager? shell;

    /// <summary>Whether the ^ item exists at all (Config.TrayIcons).</summary>
    public static bool Enabled;

    public static ObservableCollection<NotifyIcon>? Icons => shell?.NotificationArea.TrayIcons;

    public static void Start()
    {
        if (shell != null || !Enabled) return;
        try
        {
            var cfg = ShellManager.DefaultShellConfig;
            cfg.EnableTasksService = false;
            cfg.AutoStartTasksService = false;
            cfg.EnableTrayService = true;
            cfg.AutoStartTrayService = true;
            cfg.PinnedNotifyIcons = Array.Empty<string>();
            shell = new ShellManager(cfg);
        }
        catch (Exception ex)
        {
            Log(ex);
            shell = null;
        }
    }

    public static void Stop()
    {
        try { shell?.Dispose(); } catch (Exception ex) { Log(ex); }
        shell = null;
    }

    static void Log(Exception ex)
    {
        try
        {
            File.AppendAllText(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MacDock", "error.log"),
                $"{DateTime.Now:s} tray: {ex}\n\n");
        }
        catch { }
    }
}
