# Enumerate top-level windows and report the flags Chromium's occlusion tracker
# would see: visible / iconic (minimized) / rect / owning process.
# Used by the glass-to-glass harness to tell "product is slow" from "the viewer
# window is not on screen at all".
param([string]$Match = "msedge", [switch]$Restore, [switch]$All, [int]$Limit = 12)
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinInfo {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left, top, right, bottom; }
  public static List<string> Dump(string needle, bool restore, bool all, int limit) {
    var outp = new List<string>();
    int[] z = new int[1];
    EnumWindows((h, p) => {
      z[0]++;
      if (outp.Count >= limit) return false;
      uint pid; GetWindowThreadProcessId(h, out pid);
      string proc = "";
      try { proc = System.Diagnostics.Process.GetProcessById((int)pid).ProcessName; } catch { }
      if (all) { if (!IsWindowVisible(h)) return true; }
      else if (proc.IndexOf(needle, StringComparison.OrdinalIgnoreCase) < 0) return true;
      var sb = new StringBuilder(512);
      GetWindowText(h, sb, sb.Capacity);
      RECT r; GetWindowRect(h, out r);
      string title = sb.ToString();
      if (all && (title.Length == 0 || r.right - r.left < 120)) return true;
      if (title.Length == 0) return true;
      if (restore) { ShowWindow(h, 9); SetForegroundWindow(h); }
      outp.Add("z" + z[0] + " " + proc + " pid=" + pid + " vis=" + IsWindowVisible(h) + " min=" + IsIconic(h) +
               " rect=(" + r.left + "," + r.top + ")-(" + r.right + "," + r.bottom + ") title=" + title);
      return true;
    }, IntPtr.Zero);
    return outp;
  }
}
"@
[WinInfo]::Dump($Match, $Restore.IsPresent, $All.IsPresent, $Limit) | ForEach-Object { $_ }
