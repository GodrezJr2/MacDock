using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;

namespace MacDock;

class Config
{
    public List<string> Pinned { get; set; } = new()
    {
        @"shell:AppsFolder\Microsoft.Windows.Explorer",
        @"shell:AppsFolder\Chrome",
        @"shell:AppsFolder\Microsoft.OutlookForWindows_8wekyb3d8bbwe!Microsoft.OutlookforWindows",
        @"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
        @"shell:AppsFolder\Microsoft.VisualStudioCode",
        @"shell:AppsFolder\com.squirrel.Discord.Discord",
        @"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
    };
    public double IconSize { get; set; } = 52;
    public double Magnify { get; set; } = 1.75;
    public bool HideTaskbar { get; set; } = true;
    public bool MenuBar { get; set; } = true;

    static string Dir => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MacDock");
    static string FilePath => Path.Combine(Dir, "config.json");
    static readonly JsonSerializerOptions opts = new() { WriteIndented = true };

    public static Config Load()
    {
        try { if (File.Exists(FilePath)) return JsonSerializer.Deserialize<Config>(File.ReadAllText(FilePath)) ?? new Config(); }
        catch { }
        var c = new Config();
        c.Save();
        return c;
    }

    public void Save()
    {
        Directory.CreateDirectory(Dir);
        File.WriteAllText(FilePath, JsonSerializer.Serialize(this, opts));
    }
}
