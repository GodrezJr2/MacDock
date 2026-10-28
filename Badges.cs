using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace MacDock;

/// <summary>
/// Red unread badges for dock icons. Windows keeps app badges inside its taskbar (not readable while it is
/// hidden), so the count comes from two places apps do expose: a "(N) " prefix in a window title (WhatsApp
/// Web, Gmail, many chat apps) and toasts still waiting in the Action Center that arrived after the app
/// was last in front.
/// </summary>
static class Badges
{
    static readonly Regex titleCount = new(@"^\((\d{1,4})\+?\)\s", RegexOptions.Compiled);

    public static int FromTitle(string title)
    {
        var m = titleCount.Match(title);
        return m.Success ? int.Parse(m.Groups[1].Value) : 0;
    }

    // ---------- Action Center toasts (%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db)

    const string Sqlite = "winsqlite3.dll"; // ships with Windows
    [DllImport(Sqlite, CharSet = CharSet.Ansi)] static extern int sqlite3_open_v2(byte[] file, out IntPtr db, int flags, IntPtr vfs);
    [DllImport(Sqlite)] static extern int sqlite3_close(IntPtr db);
    [DllImport(Sqlite)] static extern int sqlite3_busy_timeout(IntPtr db, int ms);
    [DllImport(Sqlite)] static extern int sqlite3_prepare_v2(IntPtr db, byte[] sql, int n, out IntPtr stmt, IntPtr tail);
    [DllImport(Sqlite)] static extern int sqlite3_step(IntPtr stmt);
    [DllImport(Sqlite)] static extern int sqlite3_finalize(IntPtr stmt);
    [DllImport(Sqlite)] static extern IntPtr sqlite3_column_text(IntPtr stmt, int col);
    [DllImport(Sqlite)] static extern long sqlite3_column_int64(IntPtr stmt, int col);
    const int SQLITE_ROW = 100, SQLITE_OPEN_READONLY = 1;

    static readonly string DbPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), @"Microsoft\Windows\Notifications\wpndatabase.db");

    static byte[] Utf8(string s) => System.Text.Encoding.UTF8.GetBytes(s + "\0");

    /// <summary>App id (AUMID) and arrival time (FILETIME UTC) of every toast in the Action Center.</summary>
    public static List<(string App, long Arrived)> Toasts()
    {
        var list = new List<(string, long)>();
        if (!File.Exists(DbPath) || sqlite3_open_v2(Utf8(DbPath), out var db, SQLITE_OPEN_READONLY, IntPtr.Zero) != 0) return list;
        try
        {
            sqlite3_busy_timeout(db, 200);
            const string sql = "select h.PrimaryId, n.ArrivalTime from Notification n join NotificationHandler h on h.RecordId = n.HandlerId where n.Type = 'toast'";
            if (sqlite3_prepare_v2(db, Utf8(sql), -1, out var st, IntPtr.Zero) != 0) return list;
            try
            {
                while (sqlite3_step(st) == SQLITE_ROW)
                    if (Marshal.PtrToStringUTF8(sqlite3_column_text(st, 0)) is string app) list.Add((app, sqlite3_column_int64(st, 1)));
            }
            finally { sqlite3_finalize(st); }
        }
        catch { }
        finally { sqlite3_close(db); }
        return list;
    }

    // ---------- when each app was last looked at, so toasts already seen don't keep a badge up

    static readonly string SeenPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MacDock", "seen.json");
    static Dictionary<string, long> seen = new(StringComparer.OrdinalIgnoreCase);
    static long baseline; // first run: toasts from before MacDock knew about them don't count

    public static void Load()
    {
        try
        {
            var s = JsonSerializer.Deserialize<Dictionary<string, long>>(File.ReadAllText(SeenPath));
            if (s != null) seen = new(s, StringComparer.OrdinalIgnoreCase);
        }
        catch { }
        if (!seen.TryGetValue("*", out baseline)) { baseline = DateTime.UtcNow.ToFileTimeUtc(); seen["*"] = baseline; Save(); }
    }

    static void Save()
    {
        try { Directory.CreateDirectory(Path.GetDirectoryName(SeenPath)!); File.WriteAllText(SeenPath, JsonSerializer.Serialize(seen)); } catch { }
    }

    public static long SeenAt(string app) => seen.TryGetValue(app, out var t) ? t : baseline;

    public static void MarkSeen(string app)
    {
        seen[app] = DateTime.UtcNow.ToFileTimeUtc();
        Save();
    }
}
