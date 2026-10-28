using System;
using System.Collections.Generic;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Shapes;

namespace MacDock;

class DockItem
{
    public required string Key;
    public string Name = "";
    public ImageSource? Icon;
    public string? Launch;   // what to start: shell:AppsFolder\…, .lnk, .exe
    public string? Aumid;
    public string? Exe;
    public bool Pinned;
    public bool IsTrash;
    public List<AppWindow> Windows = new();

    // animation state (all in DIPs)
    public double Scale = 1;          // magnification, smoothed
    public double Presence;           // 0 → 1 when it appears, back to 0 before removal
    public bool Leaving;
    public double X, Size;            // laid out this frame
    public double BounceT;            // seconds into the current bounce cycle, 0 = idle
    public bool Launching;
    public DateTime LaunchStarted;
    public int WindowsAtLaunch;
    public bool Attention;            // the app flashed its taskbar button: bounce until it comes to front
    public DateTime AttentionStarted;
    public bool AttentionBounce;      // BounceT is running an attention cycle (finishes the hop after it stops)
    public double DotAlpha;

    public Image? Img;
    public Ellipse? Dot;

    public bool Running => Windows.Count > 0;

    public bool Owns(AppWindow w) =>
        (Aumid != null && w.Aumid != null && string.Equals(Aumid, w.Aumid, StringComparison.OrdinalIgnoreCase)) ||
        (Exe != null && w.Exe != null && string.Equals(Exe, w.Exe, StringComparison.OrdinalIgnoreCase));
}
