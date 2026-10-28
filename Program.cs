using System;
using System.IO;
using System.Linq;
using System.Threading;
using System.Windows;

namespace MacDock;

static class Program
{
    [STAThread]
    static void Main(string[] args)
    {
        if (args.Contains("--restore"))
        {
            // escape hatch if the dock was killed while the taskbar was hidden
            Taskbar.Restore(force: true);
            return;
        }
        using var mutex = new Mutex(true, "MacDock.Singleton", out bool first);
        if (!first) return;
        new App().Run();
    }
}

class App : Application
{
    DockWindow? dock;
    MenuBarWindow? bar;
    Config cfg = new();

    static string LogPath => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MacDock", "error.log");

    protected override void OnStartup(StartupEventArgs e)
    {
        ShutdownMode = ShutdownMode.OnExplicitShutdown;
        Resources.MergedDictionaries.Add(new ResourceDictionary { Source = new Uri("pack://application:,,,/MacDock;component/MenuStyle.xaml") });

        DispatcherUnhandledException += (_, ev) =>
        {
            try { File.AppendAllText(LogPath, $"{DateTime.Now:s} {ev.Exception}\n\n"); } catch { }
            ev.Handled = true;
        };
        AppDomain.CurrentDomain.UnhandledException += (_, _) => Taskbar.Restore();
        SessionEnding += (_, _) => Taskbar.Restore();

        cfg = Config.Load();
        if (cfg.HideTaskbar) Taskbar.Hide();
        if (cfg.MenuBar) { bar = new MenuBarWindow(); bar.Show(); }
        dock = new DockWindow(cfg);
        dock.Start();
    }

    public static void Fullscreen(bool on)
    {
        if (Current is not App app) return;
        app.dock?.SetFullscreenHidden(on);
        if (app.bar != null) app.bar.Visibility = on ? Visibility.Hidden : Visibility.Visible;
    }

    public static void Quit()
    {
        if (Current is not App app) return;
        app.dock?.Shutdown();
        app.bar?.Shutdown();
        Taskbar.Restore();
        app.Shutdown();
    }

    protected override void OnExit(ExitEventArgs e)
    {
        Taskbar.Restore();
        base.OnExit(e);
    }
}
