#Requires -Version 5.1
<##
.SYNOPSIS
  Native Windows feedback loop for the hover-tab target and geometry contract.

.DESCRIPTION
  This is an operator smoke, not a DOM test. It starts a private sacrificial
  only when -LaunchSacrificial is supplied, moves the real cursor, observes the
  topmost HWND and the Petal Hover Tab HWND, and records bounded native facts.
  Run it while the Petal process identified by -PetalPid is already in a
  meeting. The process gate accepts exactly that owned Petal process and
  refuses to exercise the loop while another Petal instance is present.

  The fixed-tab contract proves that ordinary and maximized windows keep a
  40x40 tab after dwell and every activation. Right-click opens the native
  options menu without toggling sharing; Escape closes that menu. With
  -ExerciseShare -LaunchSacrificial, the smoke checks Share->Stop->Share->Stop
  reuse separately in ordinary/outside and maximized/inset placement and
  selects a quality priority through the native menu. With -ExercisePosition,
  it selects a Top/Center/Bottom placement preset and verifies the persisted
  JSON offset plus resulting native geometry. With -ExerciseFollow, it first
  proves the native tab-edge detector goes red after a deliberate tab offset,
  then samples the source, Petal border, and tab every 8ms during continuous
  movement. With -ExerciseOcclusion, it creates a private normal-band
  occluder over the tab while the cursor stays on a visible source region,
  verifies occluder -> tab -> source order, and restores the tab. Shell-
  surface negative controls remain available.

.EXAMPLE
  .\windows-hover-tab-smoke.ps1 -PetalPid 1234 -LaunchSacrificial

.EXAMPLE
  .\windows-hover-tab-smoke.ps1 -PetalPid 1234 -LaunchSacrificial -Surface start

.EXAMPLE
  .\windows-hover-tab-smoke.ps1 -PetalPid 1234 -Surface all
#>

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [Alias('PetalPid', 'OwnedPid')]
  [ValidateRange(1, 2147483647)]
  [int]$OwnedPetalPid,
  [long]$TargetHwnd = 0,
  [Alias('LaunchNotepad')]
  [switch]$LaunchSacrificial,
  [ValidateSet('none', 'current', 'start', 'notifications', 'quick-settings', 'taskbar', 'all')]
  [string]$Surface = 'none',
  [ValidateRange(1, 30)] [int]$TimeoutSeconds = 8,
  [string]$EvidencePath = '',
  [Alias('KeepNotepad')]
  [switch]$KeepSacrificial,
  [switch]$ExerciseShare,
  [switch]$ExerciseFollow,
  [switch]$ExercisePosition,
  [switch]$ExerciseOcclusion
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# Insurance against dev.sh's broad cleanup: accept exactly the explicitly-owned
# Petal process, verify its executable metadata, and reject every other Petal
# process. The check is intentionally repeated immediately before the operator
# launches this script; a stale earlier clearance is not evidence.
$petalProcessPattern = '^(desktop|Petal)\.exe$'
$petalProcesses = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
  $_.Name -match $petalProcessPattern
})
if ($OwnedPetalPid -le 0) {
  throw 'FATAL: -PetalPid is required and must identify the Petal process this smoke is allowed to use.'
}
$ownedCandidates = @($petalProcesses | Where-Object { [int]$_.ProcessId -eq $OwnedPetalPid })
if ($ownedCandidates.Count -ne 1) {
  $seen = if ($petalProcesses.Count -eq 0) { 'none' } else {
    ($petalProcesses | ForEach-Object { "PID=$($_.ProcessId) path=$($_.ExecutablePath)" }) -join '; '
  }
  throw "FATAL: -PetalPid $OwnedPetalPid is not a running Petal binary (running Petal processes: $seen)"
}
$ownedProcess = $ownedCandidates[0]
$ownedPath = [string]$ownedProcess.ExecutablePath
if ([string]::IsNullOrWhiteSpace($ownedPath) -or -not (Test-Path -LiteralPath $ownedPath -PathType Leaf)) {
  throw "FATAL: owned Petal PID $OwnedPetalPid has no readable executable path: '$ownedPath'"
}
$ownedFile = Get-Item -LiteralPath $ownedPath
$versionInfo = $ownedFile.VersionInfo
$isPetalBinary = ([IO.Path]::GetFileName($ownedPath) -match $petalProcessPattern) -and
  ([string]$versionInfo.ProductName -eq 'Petal' -or [string]$versionInfo.FileDescription -eq 'Petal')
if (-not $isPetalBinary) {
  throw "FATAL: -PetalPid $OwnedPetalPid is not verified as a Petal binary (path='$ownedPath', product='$($versionInfo.ProductName)', description='$($versionInfo.FileDescription)')"
}
$foreignPetal = @($petalProcesses | Where-Object { [int]$_.ProcessId -ne $OwnedPetalPid })
if ($foreignPetal.Count -gt 0) {
  $paths = ($foreignPetal | ForEach-Object { "PID=$($_.ProcessId) path=$($_.ExecutablePath)" }) -join '; '
  throw "FATAL: another Petal binary is already running -- only PID $OwnedPetalPid is allowed ($paths)"
}

# Set the diagnostic self PID before the first Fact/classifier call. This is
# deliberately not inferred from the Hover Tab HWND after positive control.
$petalPid = [uint32]$OwnedPetalPid
if ($ExerciseFollow -and -not $LaunchSacrificial) {
  throw '-ExerciseFollow requires -LaunchSacrificial so the harness moves only its private source window.'
}
if ($ExerciseFollow -and -not $ExerciseShare) {
  throw '-ExerciseFollow requires -ExerciseShare so the Petal border and hover tab can be compared.'
}
if ($ExerciseOcclusion -and -not $LaunchSacrificial) {
  throw '-ExerciseOcclusion requires -LaunchSacrificial so the harness owns both source and occluder windows.'
}

if (-not $EvidencePath) {
  $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
  $EvidencePath = Join-Path ([IO.Path]::GetTempPath()) "petal-hover-tab-$stamp.json"
}

# PowerShell 7 does not include the threading/process facades when an
# explicit Add-Type reference list is supplied; include them so the same
# harness compiles under both Windows PowerShell and pwsh.
$compiledReferences = @(
  'System'
  'System.Collections'
  'System.Core'
  'System.Windows.Forms'
  'System.Drawing'
  'System.Drawing.Primitives'
  'System.Runtime'
  'System.Threading'
  'System.Threading.Thread'
  'System.Diagnostics.Process'
  'System.ComponentModel.Primitives'
)
$privateWindowsCoreAssembly = [AppDomain]::CurrentDomain.GetAssemblies() |
  Where-Object { $_.GetName().Name -eq 'System.Private.Windows.Core' } |
  Select-Object -First 1 |
  ForEach-Object { $_.Location }
if ($privateWindowsCoreAssembly) { $compiledReferences += $privateWindowsCoreAssembly }
Add-Type -ReferencedAssemblies $compiledReferences -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class PetalHoverTabSmoke {
    private const uint GW_OWNER = 4;
    private const uint GW_HWNDPREV = 3;
    private const uint GA_ROOT = 2;
    private const uint GA_ROOTOWNER = 3;
    private const int GWL_STYLE = -16;
    private const int GWL_EXSTYLE = -20;
    private const long WS_EX_TOOLWINDOW = 0x00000080L;
    private const long WS_EX_APPWINDOW = 0x00040000L;
    private const uint DWMWA_EXTENDED_FRAME_BOUNDS = 9;
    private const uint DWMWA_CLOAKED = 14;
    private const int SW_MAXIMIZE = 3;
    private const int SW_HIDE = 0;
    private const uint SWP_NOZORDER = 0x0004;
    private const uint SWP_NOACTIVATE = 0x0010;
    private const uint SWP_NOOWNERZORDER = 0x0200;
    private const uint SWP_SHOWWINDOW = 0x0040;
    private const uint KEYEVENTF_KEYUP = 0x0002;
    private const uint INPUT_MOUSE = 0;
    private const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    private const uint MOUSEEVENTF_LEFTUP = 0x0004;
    private const uint MOUSEEVENTF_RIGHTDOWN = 0x0008;
    private const uint MOUSEEVENTF_RIGHTUP = 0x0010;
    // Keep synthetic shell/menu input held briefly so native event delivery
    // observes the same edge a human input event produces.
    private const int SYNTHETIC_INPUT_HOLD_MS = 80;

    [StructLayout(LayoutKind.Sequential)]
    public struct Point { public int X; public int Y; }
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left; public int Top; public int Right; public int Bottom; }
    [StructLayout(LayoutKind.Sequential)]
    private struct MouseInput {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }
    [StructLayout(LayoutKind.Explicit, Size = 40)]
    private struct Input {
        [FieldOffset(0)] public uint Type;
        [FieldOffset(8)] public MouseInput Mouse;
    }
    public sealed class WindowFact {
        public string hwnd;
        public string childHwnd;
        public string className;
        public string title;
        public bool hasTitle;
        public uint pid;
        public string processName;
        public string root;
        public string rootOwner;
        public string owner;
        public long style;
        public long exStyle;
        public bool toolWindow;
        public bool appWindow;
        public bool visible;
        public bool minimized;
        public bool maximized;
        public bool cloaked;
        public int x;
        public int y;
        public int width;
        public int height;
        public int dpi;
        public string pickerDecision;
        public string observedAtUtc;
    }

    private delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassName(IntPtr hwnd, StringBuilder text, int maxCount);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int maxCount);
    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern bool IsZoomed(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")]
    private static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")]
    private static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out Point point);
    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    private static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetWindowPos(
        IntPtr hwnd,
        IntPtr insertAfter,
        int x,
        int y,
        int width,
        int height,
        uint flags);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    private static extern IntPtr GetWindowLongPtr(IntPtr hwnd, int index);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    private static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("user32.dll")]
    private static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")]
    private static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    private static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint SendInput(uint count, Input[] inputs, int inputSize);
    [DllImport("dwmapi.dll")]
    private static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attribute, out Rect value, int valueSize);
    [DllImport("dwmapi.dll")]
    private static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attribute, out int value, int valueSize);

    public static bool MoveCursor(int x, int y) { return SetCursorPos(x, y); }
    public static bool SetWindowFrame(IntPtr hwnd, int x, int y, int width, int height) {
        return SetWindowPos(hwnd, IntPtr.Zero, x, y, width, height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
    }

    public static void PressVirtualKey(uint key) {
        keybd_event((byte)key, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(SYNTHETIC_INPUT_HOLD_MS);
        keybd_event((byte)key, 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
    }

    public static void PressChord(uint modifier, uint key) {
        keybd_event((byte)modifier, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(SYNTHETIC_INPUT_HOLD_MS / 2);
        keybd_event((byte)key, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(SYNTHETIC_INPUT_HOLD_MS);
        keybd_event((byte)key, 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
        keybd_event((byte)modifier, 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
    }

    public static bool ActivateWindow(IntPtr hwnd) {
        var foreground = GetForegroundWindow();
        uint foregroundPid;
        uint targetPid;
        uint foregroundThread = foreground == IntPtr.Zero
            ? 0
            : GetWindowThreadProcessId(foreground, out foregroundPid);
        uint targetThread = GetWindowThreadProcessId(hwnd, out targetPid);
        bool attached = foregroundThread != 0 && targetThread != 0 &&
            foregroundThread != targetThread &&
            AttachThreadInput(foregroundThread, targetThread, true);
        try {
            BringWindowToTop(hwnd);
            ShowWindow(hwnd, 5);
            return SetForegroundWindow(hwnd);
        } finally {
            if (attached) AttachThreadInput(foregroundThread, targetThread, false);
        }
    }

    public static bool Maximize(IntPtr hwnd) { return ShowWindow(hwnd, SW_MAXIMIZE); }

    public static bool ClickAt(int x, int y) {
        return ClickButtonAt(x, y, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP);
    }

    public static bool RightClickAt(int x, int y) {
        return ClickButtonAt(x, y, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP);
    }

    private static bool SendMouseButton(uint flags) {
        var input = new[] {
            new Input { Type = INPUT_MOUSE, Mouse = new MouseInput { Flags = flags } }
        };
        return SendInput(1, input, Marshal.SizeOf(typeof(Input))) == 1;
    }

    private static bool ClickButtonAt(int x, int y, uint downFlag, uint upFlag) {
        if (!SetCursorPos(x, y)) return false;
        if (!SendMouseButton(downFlag)) return false;
        System.Threading.Thread.Sleep(SYNTHETIC_INPUT_HOLD_MS);
        return SendMouseButton(upFlag);
    }

    private static System.Windows.Forms.Form sacrificialForm;
    private static System.Threading.Thread sacrificialThread;
    private static readonly System.Threading.ManualResetEventSlim sacrificialReady =
        new System.Threading.ManualResetEventSlim(false);
    private static System.Windows.Forms.Form occluderForm;
    private static System.Threading.Thread occluderThread;
    private static readonly System.Threading.ManualResetEventSlim occluderReady =
        new System.Threading.ManualResetEventSlim(false);
    // A private WinForms window keeps the positive control inside this
    // PowerShell process. It avoids borrowing a Windows Terminal tab or
    // killing a process the operator did not start.
    public static IntPtr StartSacrificialWindow() {
        sacrificialReady.Reset();
        sacrificialThread = new System.Threading.Thread(() => {
            sacrificialForm = new System.Windows.Forms.Form {
                Text = "Petal Hover Smoke",
                Width = 800,
                Height = 600,
                ShowInTaskbar = true,
                StartPosition = System.Windows.Forms.FormStartPosition.CenterScreen
            };
            sacrificialForm.Shown += (_, __) => sacrificialReady.Set();
            System.Windows.Forms.Application.Run(sacrificialForm);
        });
        sacrificialThread.IsBackground = true;
        sacrificialThread.SetApartmentState(System.Threading.ApartmentState.STA);
        sacrificialThread.Start();
        if (!sacrificialReady.Wait(TimeSpan.FromSeconds(5))) {
            StopSacrificialWindow();
            WaitForSacrificialStopped(5000);
            return IntPtr.Zero;
        }
        return sacrificialForm.Handle;
    }

    public static void StopSacrificialWindow() {
        var form = sacrificialForm;
        if (form == null || form.IsDisposed) return;
        try { form.BeginInvoke(new Action(() => form.Close())); } catch { }
    }

    public static bool WaitForSacrificialStopped(int timeoutMs) {
        var thread = sacrificialThread;
        return thread == null || !thread.IsAlive || thread.Join(timeoutMs);
    }

    public static IntPtr StartOccluderWindow() {
        occluderReady.Reset();
        occluderThread = new System.Threading.Thread(() => {
            occluderForm = new System.Windows.Forms.Form {
                Text = "Petal Hover Smoke Occluder",
                Width = 80,
                Height = 80,
                ShowInTaskbar = false,
                FormBorderStyle = System.Windows.Forms.FormBorderStyle.None,
                StartPosition = System.Windows.Forms.FormStartPosition.Manual,
                Visible = false,
                BackColor = System.Drawing.Color.Magenta
            };
            var handle = occluderForm.Handle;
            occluderReady.Set();
            System.Windows.Forms.Application.Run(occluderForm);
        });
        occluderThread.IsBackground = true;
        occluderThread.SetApartmentState(System.Threading.ApartmentState.STA);
        occluderThread.Start();
        if (!occluderReady.Wait(TimeSpan.FromSeconds(5))) {
            StopOccluderWindow();
            WaitForOccluderStopped(5000);
            return IntPtr.Zero;
        }
        return occluderForm.Handle;
    }

    public static bool PlaceAboveNoActivate(
        IntPtr hwnd, IntPtr anchor, int x, int y, int width, int height, bool show) {
        if (hwnd == IntPtr.Zero || anchor == IntPtr.Zero) return false;
        uint flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if (show) flags |= SWP_SHOWWINDOW;
        return SetWindowPos(hwnd, anchor, x, y, width, height, flags);
    }

    public static bool HideNoActivate(IntPtr hwnd) {
        if (hwnd == IntPtr.Zero) return false;
        ShowWindow(hwnd, SW_HIDE);
        return true;
    }

    public static void StopOccluderWindow() {
        var form = occluderForm;
        if (form == null || form.IsDisposed) return;
        try { form.BeginInvoke(new Action(() => form.Close())); } catch { }
    }

    public static bool WaitForOccluderStopped(int timeoutMs) {
        var thread = occluderThread;
        return thread == null || !thread.IsAlive || thread.Join(timeoutMs);
    }

    public static bool IsAboveInZOrder(IntPtr upper, IntPtr lower) {
        if (upper == IntPtr.Zero || lower == IntPtr.Zero || upper == lower) return false;
        var current = GetWindow(lower, GW_HWNDPREV);
        for (var index = 0; index < 256 && current != IntPtr.Zero; index++) {
            if (current == upper) return true;
            current = GetWindow(current, GW_HWNDPREV);
        }
        return false;
    }

    public static IntPtr FindVisibleTitle(string prefix) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            if (!IsWindowVisible(hwnd)) return true;
            var text = new StringBuilder(512);
            GetWindowText(hwnd, text, text.Capacity);
            if (text.ToString().StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) {
                result = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero);
        return result;
    }

    public static IntPtr FindVisibleClass(string className) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            if (!IsWindowVisible(hwnd)) return true;
            if (ReadClassName(hwnd).Equals(className, StringComparison.OrdinalIgnoreCase)) {
                result = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero);
        return result;
    }

    private static string ReadClassName(IntPtr hwnd) {
        var text = new StringBuilder(256);
        GetClassName(hwnd, text, text.Capacity);
        return text.ToString();
    }

    private static string ReadProcessName(IntPtr hwnd) {
        uint pid;
        GetWindowThreadProcessId(hwnd, out pid);
        try { return System.Diagnostics.Process.GetProcessById((int)pid).ProcessName; }
        catch { return "unknown"; }
    }

    private static string NormalizeProcessName(string processName) {
        var value = (processName ?? string.Empty).Trim();
        return value.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)
            ? value.Substring(0, value.Length - 4)
            : value;
    }

    private static bool IsKnownShellProcess(string processName) {
        var proc = NormalizeProcessName(processName);
        return proc.Equals("ShellExperienceHost", StringComparison.OrdinalIgnoreCase) ||
            proc.Equals("StartMenuExperienceHost", StringComparison.OrdinalIgnoreCase) ||
            proc.Equals("SearchHost", StringComparison.OrdinalIgnoreCase) ||
            proc.Equals("TextInputHost", StringComparison.OrdinalIgnoreCase);
    }

    private static bool SurfaceMatches(IntPtr hwnd, string requested) {
        if (hwnd == IntPtr.Zero || !IsWindowVisible(hwnd)) return false;
        var cls = ReadClassName(hwnd).Trim();
        var proc = NormalizeProcessName(ReadProcessName(hwnd));
        var surface = (requested ?? string.Empty).Trim().ToLowerInvariant();
        if (surface == "taskbar") {
            return cls.Equals("Shell_TrayWnd", StringComparison.OrdinalIgnoreCase) ||
                cls.Equals("Shell_SecondaryTrayWnd", StringComparison.OrdinalIgnoreCase);
        }
        if (surface == "start") {
            return proc.Equals("StartMenuExperienceHost", StringComparison.OrdinalIgnoreCase) ||
                cls.Equals("XamlExplorerHostIslandWindow", StringComparison.OrdinalIgnoreCase);
        }
        if (surface == "notifications") {
            // TextInputHost also owns a full-screen CoreWindow, but it is not
            // the notification flyout.
            return proc.Equals("ShellExperienceHost", StringComparison.OrdinalIgnoreCase) &&
                (cls.Equals("Windows.UI.Core.CoreWindow", StringComparison.OrdinalIgnoreCase) ||
                 cls.Equals("XamlExplorerHostIslandWindow", StringComparison.OrdinalIgnoreCase));
        }
        if (surface == "quick-settings") {
            // Windows 11 uses ShellHost/ControlCenterWindow for Quick
            // Settings, while older builds use ShellExperienceHost.
            return (proc.Equals("ShellHost", StringComparison.OrdinalIgnoreCase) &&
                    cls.Equals("ControlCenterWindow", StringComparison.OrdinalIgnoreCase)) ||
                (proc.Equals("ShellExperienceHost", StringComparison.OrdinalIgnoreCase) &&
                 (cls.Equals("Windows.UI.Core.CoreWindow", StringComparison.OrdinalIgnoreCase) ||
                  cls.Equals("XamlExplorerHostIslandWindow", StringComparison.OrdinalIgnoreCase)));
        }
        return false;
    }

    private static IntPtr SurfaceAtPoint(Point point, string requested) {
        var child = WindowFromPoint(point);
        var root = GetAncestor(child, GA_ROOT);
        return SurfaceMatches(root, requested) ? root : IntPtr.Zero;
    }

    // Find the newly opened shell HWND, preferring the foreground root and then
    // the topmost visible EnumWindows result. Some Windows 11 shell flyouts
    // are visible to WindowFromPoint but omitted from EnumWindows, so finish
    // with screen probes. The caller moves the real cursor into this HWND
    // before collecting the diagnostic facts.
    public static IntPtr FindVisibleShellSurface(string requested) {
        var foreground = GetAncestor(GetForegroundWindow(), GA_ROOT);
        if (SurfaceMatches(foreground, requested)) return foreground;
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            if (SurfaceMatches(hwnd, requested)) { result = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        if (result != IntPtr.Zero) return result;

        foreach (var screen in System.Windows.Forms.Screen.AllScreens) {
            var bounds = screen.Bounds;
            var probes = new[] {
                new Point { X = bounds.Left + bounds.Width / 2, Y = bounds.Top + bounds.Height / 2 },
                new Point { X = bounds.Right - 80, Y = bounds.Bottom - 120 },
                new Point { X = bounds.Right - 320, Y = bounds.Bottom - 240 },
                new Point { X = bounds.Left + 80, Y = bounds.Bottom - 120 },
                new Point { X = bounds.Right - 80, Y = bounds.Top + 80 },
                // ShellHost's Quick Settings HWND is often omitted from
                // EnumWindows; probe the whole tray-side panel, not just one
                // point that can land in an empty corner of the flyout.
                new Point { X = bounds.Right - 40, Y = bounds.Bottom - 40 },
                new Point { X = bounds.Right - 120, Y = bounds.Bottom - 40 },
                new Point { X = bounds.Right - 40, Y = bounds.Bottom - 160 },
                new Point { X = bounds.Right - 240, Y = bounds.Bottom - 160 }
            };
            foreach (var point in probes) {
                result = SurfaceAtPoint(point, requested);
                if (result != IntPtr.Zero) return result;
            }
        }
        return IntPtr.Zero;
    }

    public static IntPtr TopmostAtCursor() {
        Point point;
        return GetCursorPos(out point) ? WindowFromPoint(point) : IntPtr.Zero;
    }

    public static WindowFact FactAtCursor(uint selfPid) {
        Point point;
        if (!GetCursorPos(out point)) return null;
        IntPtr child = WindowFromPoint(point);
        IntPtr root = GetAncestor(child, GA_ROOT);
        return Fact(root, selfPid, child);
    }

    // Diagnostic-only mirror of share_target.rs. The Rust classifier remains
    // authoritative; this keeps JSON evidence useful when a shell HWND is
    // observed without exposing a second production policy path.
    private static bool IsKnownSystemSurface(string className, string processName) {
        var cls = (className ?? string.Empty).Trim();
        var proc = NormalizeProcessName(processName);
        if (cls.Equals("#32768", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("tooltips_class32", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("Shell_TrayWnd", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("Shell_SecondaryTrayWnd", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("NotifyIconOverflowWindow", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("TopLevelWindowForOverflowXamlIsland", StringComparison.OrdinalIgnoreCase) ||
            cls.Equals("XamlExplorerHostIslandWindow", StringComparison.OrdinalIgnoreCase)) return true;
        if (cls.Equals("ControlCenterWindow", StringComparison.OrdinalIgnoreCase) &&
            proc.Equals("ShellHost", StringComparison.OrdinalIgnoreCase)) return true;
        if (!cls.Equals("Windows.UI.Core.CoreWindow", StringComparison.OrdinalIgnoreCase)) return false;
        return IsKnownShellProcess(proc);
    }

    private static string PickerDecision(
        uint pid, uint selfPid, bool visible, bool minimized, bool cloaked,
        long exStyle, IntPtr owner, IntPtr root, IntPtr rootOwner,
        int width, int height, string className, string processName) {
        if (pid == 0) return "rejected-unknown-owner";
        if (!visible) return "rejected-hidden";
        if (minimized) return "rejected-minimized";
        if (cloaked) return "rejected-cloaked";
        if (width < 40 || height < 40) return "rejected-too-small";
        if (pid == selfPid) return "rejected-own-petal-window";
        if (owner != IntPtr.Zero || (root != IntPtr.Zero && rootOwner != IntPtr.Zero && root != rootOwner)) return "rejected-owned-or-transient";
        if (IsKnownSystemSurface(className, processName)) return "rejected-system-surface";
        if ((exStyle & WS_EX_TOOLWINDOW) != 0) return "rejected-tool-window";
        return "eligible-by-share-target-classifier";
    }

    public static WindowFact Fact(IntPtr hwnd, uint selfPid, IntPtr child) {
        if (hwnd == IntPtr.Zero) return null;
        uint pid;
        GetWindowThreadProcessId(hwnd, out pid);
        IntPtr root = GetAncestor(hwnd, GA_ROOT);
        IntPtr rootOwner = GetAncestor(hwnd, GA_ROOTOWNER);
        IntPtr owner = GetWindow(hwnd, GW_OWNER);
        var titleBuffer = new StringBuilder(512);
        GetWindowText(hwnd, titleBuffer, titleBuffer.Capacity);
        var classBuffer = new StringBuilder(256);
        GetClassName(hwnd, classBuffer, classBuffer.Capacity);
        Rect rect;
        if (!GetWindowRect(hwnd, out rect)) rect = new Rect();
        Rect frame;
        if (DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, out frame, Marshal.SizeOf(typeof(Rect))) != 0)
            frame = rect;
        int cloak;
        bool cloaked = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, out cloak, sizeof(int)) == 0 && cloak != 0;
        long style = GetWindowLongPtr(hwnd, GWL_STYLE).ToInt64();
        long exStyle = GetWindowLongPtr(hwnd, GWL_EXSTYLE).ToInt64();
        bool visible = IsWindowVisible(hwnd);
        bool minimized = IsIconic(hwnd);
        int width = Math.Max(0, frame.Right - frame.Left);
        int height = Math.Max(0, frame.Bottom - frame.Top);
        int dpi = (int)Math.Max(96, GetDpiForWindow(hwnd));
        string processName = ReadProcessName(hwnd);
        string pickerDecision = PickerDecision(
            pid, selfPid, visible, minimized, cloaked, exStyle, owner,
            root, rootOwner, width, height, classBuffer.ToString(), processName);
        return new WindowFact {
            hwnd = Format(hwnd), childHwnd = Format(child),
            className = classBuffer.ToString(), title = titleBuffer.ToString(),
            hasTitle = titleBuffer.Length > 0, pid = pid, processName = processName,
            root = Format(root), rootOwner = Format(rootOwner), owner = Format(owner),
            style = style, exStyle = exStyle,
            toolWindow = (exStyle & WS_EX_TOOLWINDOW) != 0,
            appWindow = (exStyle & WS_EX_APPWINDOW) != 0,
            visible = visible, minimized = minimized, maximized = IsZoomed(hwnd), cloaked = cloaked,
            x = frame.Left, y = frame.Top, width = width, height = height, dpi = dpi,
            pickerDecision = pickerDecision,
            observedAtUtc = DateTime.UtcNow.ToString("o")
        };
    }

    private static string Format(IntPtr value) { return value == IntPtr.Zero ? "0x0" : "0x" + value.ToInt64().ToString("X"); }
}
'@

function Get-Fact([IntPtr]$Hwnd, [IntPtr]$Child = [IntPtr]::Zero) {
  if ($Hwnd -eq [IntPtr]::Zero) { return $null }
  return [PetalHoverTabSmoke]::Fact($Hwnd, $script:petalPid, $Child)
}

function Get-SafeFact($Fact) {
  if ($null -eq $Fact) { return $null }
  # Do not persist window titles or page content in smoke evidence. HWND/class/
  # PID relationships are sufficient for this native regression.
  return [pscustomobject]@{
    hwnd = $Fact.hwnd
    childHwnd = $Fact.childHwnd
    className = $Fact.className
    hasTitle = $Fact.hasTitle
    pid = $Fact.pid
    processName = $Fact.processName
    root = $Fact.root
    rootOwner = $Fact.rootOwner
    owner = $Fact.owner
    style = $Fact.style
    exStyle = $Fact.exStyle
    toolWindow = $Fact.toolWindow
    appWindow = $Fact.appWindow
    visible = $Fact.visible
    minimized = $Fact.minimized
    maximized = $Fact.maximized
    cloaked = $Fact.cloaked
    x = $Fact.x
    y = $Fact.y
    width = $Fact.width
    height = $Fact.height
    dpi = $Fact.dpi
    pickerDecision = $Fact.pickerDecision
    observedAtUtc = $Fact.observedAtUtc
  }
}

function Wait-ForFact([scriptblock]$Read, [string]$Description) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $value = & $Read
    $empty = $null -eq $value
    if (-not $empty -and $value -is [IntPtr]) {
      $empty = $value -eq [IntPtr]::Zero
    }
    if (-not $empty) { return $value }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "$Description was not observable within $TimeoutSeconds seconds. Is Petal in a meeting?"
}

function Wait-ForHwnd([scriptblock]$Read, [string]$Description) {
  return [IntPtr](Wait-ForFact $Read $Description)
}

function Wait-ForShellSurface([string]$SurfaceName) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  $fallbackAt = [DateTime]::UtcNow.AddMilliseconds(1500)
  $fallbackAttempted = $false
  do {
    $candidate = [PetalHoverTabSmoke]::FindVisibleShellSurface($SurfaceName)
    if ($candidate -ne [IntPtr]::Zero) { return $candidate }
    if ($SurfaceName -eq 'quick-settings' -and
        -not $fallbackAttempted -and
        [DateTime]::UtcNow -ge $fallbackAt) {
      # Win+A is the primary path. Some shell builds expose the flyout only
      # after the tray's network/volume affordance receives a real click.
      $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
      $fallbackAttempted = $true
      if (-not [PetalHoverTabSmoke]::ClickAt(
          [int]($bounds.Right - 110), [int]($bounds.Bottom - 35))) {
        throw 'SendInput failed while opening the Quick Settings tray fallback.'
      }
      $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "Shell surface '$SurfaceName' was not observable within $TimeoutSeconds seconds."
}

function Center-Fact($Fact) {
  return @(
    [int]($Fact.x + $Fact.width / 2),
    [int]($Fact.y + $Fact.height / 2)
  )
}

function Get-WorkingArea([IntPtr]$Hwnd) {
  if ($Hwnd -eq [IntPtr]::Zero) { return $null }
  try {
    return [System.Windows.Forms.Screen]::FromHandle($Hwnd).WorkingArea
  } catch {
    return $null
  }
}

function Check-WorkAreaContainment($Tab, $WorkArea, [string]$Name) {
  $errors = [System.Collections.Generic.List[string]]::new()
  if ($null -eq $WorkArea) {
    [void]$errors.Add("${Name}: containing monitor work area was unavailable")
    return $errors
  }
  if ($null -eq $Tab) {
    [void]$errors.Add("${Name}: Hover Tab was not visible")
    return $errors
  }
  if ($Tab.x -lt $WorkArea.Left -or $Tab.y -lt $WorkArea.Top -or
      $Tab.x + $Tab.width -gt $WorkArea.Right -or
      $Tab.y + $Tab.height -gt $WorkArea.Bottom) {
    [void]$errors.Add("${Name}: Hover Tab $($Tab.x),$($Tab.y) $($Tab.width)x$($Tab.height) escaped Screen.WorkingArea $($WorkArea.Left),$($WorkArea.Top)-$($WorkArea.Right),$($WorkArea.Bottom)")
  }
  return $errors
}

function Wait-ForWorkAreaTab($WorkArea, [string]$Description) {
  return Wait-ForFact {
    $candidate = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
    if ($null -eq $candidate) { return $null }
    if (@(Check-WorkAreaContainment $candidate $WorkArea 'convergence').Count -eq 0) {
      return $candidate
    }
    return $null
  } $Description
}

function Expected-Pixels([double]$LogicalPixels, $Fact) {
  return [int][Math]::Round($LogicalPixels * ($Fact.dpi / 96.0))
}

function Add-Observation([string]$Name, $TargetFact, $TabFact, [hashtable]$Checks) {
  $observations.Add([pscustomobject]@{
    name = $Name
    target = Get-SafeFact $TargetFact
    hoverTab = Get-SafeFact $TabFact
    checks = [pscustomobject]$Checks
  })
}

function Check-UnifiedGeometry($Target, $Tab, [string]$Name) {
  $errors = [System.Collections.Generic.List[string]]::new()
  if ($null -eq $Tab) { $errors.Add("${Name}: Hover Tab is not visible") }
  else {
    $expectedSize = Expected-Pixels 40 $Tab
    if ($Tab.width -ne $expectedSize -or $Tab.height -ne $expectedSize) {
      $errors.Add("${Name}: expected ${expectedSize}x${expectedSize} right-center square at dpi $($Tab.dpi), observed $($Tab.width)x$($Tab.height)")
    }
    $expectedCenterY = $Target.y + [int]($Target.height / 2)
    $actualCenterY = $Tab.y + [int]($Tab.height / 2)
    $centerTolerance = [Math]::Max(3, [int][Math]::Ceiling(3 * $Tab.dpi / 96.0))
    if ([Math]::Abs($actualCenterY - $expectedCenterY) -gt $centerTolerance) {
      $errors.Add("${Name}: vertical center moved by $([Math]::Abs($actualCenterY - $expectedCenterY))px")
    }
    $rightAligned = $Tab.x -ge $Target.x + $Target.width - $expectedSize - $centerTolerance -and $Tab.x -le $Target.x + $Target.width + $centerTolerance
    if (-not $rightAligned) {
      $errors.Add("${Name}: tab is not attached to the target's right edge")
    }
  }
  return $errors
}

function Check-TabAttachment($Target, $Tab, [string]$ExpectedAttachment, [string]$Name) {
  $errors = [System.Collections.Generic.List[string]]::new()
  if ($null -eq $Tab) { $errors.Add("${Name}: Hover Tab is not visible") }
  else {
    $targetRight = $Target.x + $Target.width
    $tolerance = [Math]::Max(3, [int][Math]::Ceiling(3 * $Tab.dpi / 96.0))
    if ($ExpectedAttachment -eq 'outside') {
      $offset = [Math]::Abs($Tab.x - $targetRight)
      if ($offset -gt $tolerance) {
        $errors.Add("${Name}: expected outside attachment at target right edge, observed tab-left offset ${offset}px")
      }
    } elseif ($ExpectedAttachment -eq 'inset') {
      $offset = [Math]::Abs(($Tab.x + $Tab.width) - $targetRight)
      if ($offset -gt $tolerance) {
        $errors.Add("${Name}: expected inset attachment at target right edge, observed tab-right offset ${offset}px")
      }
    } else {
      $errors.Add("${Name}: unsupported expected attachment '$ExpectedAttachment'")
    }
  }
  return $errors
}

function Get-TabAttachment($Target, $Tab) {
  if ($null -eq $Tab) { return 'unknown' }
  $targetRight = $Target.x + $Target.width
  $tolerance = [Math]::Max(3, [int][Math]::Ceiling(3 * $Tab.dpi / 96.0))
  if ([Math]::Abs($Tab.x - $targetRight) -le $tolerance) { return 'outside' }
  if ([Math]::Abs(($Tab.x + $Tab.width) - $targetRight) -le $tolerance) { return 'inset' }
  return 'unknown'
}

function Wait-ForNativeMenu([string]$Description) {
  return Wait-ForHwnd { [PetalHoverTabSmoke]::FindVisibleClass('#32768') } $Description
}

function Get-SharePreferencePath() {
  $candidates = @()
  if ($env:APPDATA) {
    $candidates += Join-Path $env:APPDATA 'com.petal.app\share-preferences.json'
    $candidates += Join-Path $env:APPDATA 'Petal\share-preferences.json'
  }
  if ($env:LOCALAPPDATA) {
    $candidates += Join-Path $env:LOCALAPPDATA 'com.petal.app\share-preferences.json'
  }
  foreach ($candidate in $candidates) {
    if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
  }
  if ($candidates.Count -gt 0) { return $candidates[0] }
  return Join-Path ([IO.Path]::GetTempPath()) 'Petal\share-preferences.json'
}

function Read-SharePreferences() {
  $path = Get-SharePreferencePath
  if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $null }
  try {
    return (Get-Content -LiteralPath $path -Raw | ConvertFrom-Json)
  } catch {
    return $null
  }
}

function Wait-ForSharePreference([scriptblock]$Match, [string]$Description) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $value = Read-SharePreferences
    if ($null -ne $value -and (& $Match $value)) { return $value }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "$Description was not persisted within $TimeoutSeconds seconds at $(Get-SharePreferencePath)."
}

function Select-NativeMenuEntry([int]$DownCount) {
  [PetalHoverTabSmoke]::PressVirtualKey(0x24) | Out-Null # Home: first enabled item
  for ($index = 0; $index -lt $DownCount; $index++) {
    [PetalHoverTabSmoke]::PressVirtualKey(0x28) | Out-Null # Down
  }
  [PetalHoverTabSmoke]::PressVirtualKey(0x0D) | Out-Null # Enter
}

function Get-PositionGeometryError($Target, $Tab, [double]$Offset, $WorkArea = $null) {
  if ($null -eq $Target -or $null -eq $Tab) { return [double]::PositiveInfinity }
  $tabSize = Expected-Pixels 40 $Tab
  $travel = [Math]::Max(0, $Target.height - $tabSize)
  $expectedY = [int][Math]::Round($Target.y + $travel * $Offset)
  # The native projection is source-relative, then clamped to rcWork. Match
  # that second step so Bottom remains a valid expectation with a bottom taskbar.
  if ($null -ne $WorkArea) {
    $expectedY = [int][Math]::Max([int]$WorkArea.Top, [Math]::Min($expectedY, [int]$WorkArea.Bottom - $tabSize))
  }
  return [double][Math]::Abs($Tab.y - $expectedY)
}

function Invoke-NativeQualityPreset($Target) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
  $tab = Get-Fact $tabHwnd
  if ($null -eq $tab) {
    [void]$errors.Add('native-quality-preset: Hover Tab was not visible')
  } else {
    $center = Center-Fact $tab
    if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) {
      [void]$errors.Add('native-quality-preset: could not open the native menu')
    } else {
      try {
        [void](Wait-ForNativeMenu 'quality-priority native menu')
        Select-NativeMenuEntry 1 # Automatic -> Responsive
        Start-Sleep -Milliseconds 120
        [void](Wait-ForSharePreference { param($value) [string]$value.priority -eq 'responsive' } 'responsive share priority')
      } catch {
        [void]$errors.Add("native-quality-preset: $($_.Exception.Message)")
      }
    }
  }
  $preferences = Read-SharePreferences
  Add-Observation 'native-quality-preset' $Target (Get-Fact $tabHwnd) @{
    selected = 'responsive'
    preferencePath = Get-SharePreferencePath
    persisted = ($null -ne $preferences -and [string]$preferences.priority -eq 'responsive')
    errors = @($errors)
  }
  return [pscustomobject]@{ Preferences = $preferences; Errors = @($errors) }
}

function Invoke-NativePositionPreset($Target, [IntPtr]$TargetHwnd, [string]$Preset = 'bottom') {
  $errors = [System.Collections.Generic.List[string]]::new()
  $offsets = @{ top = 0.0; center = 0.5; bottom = 1.0 }
  $downCounts = @{ top = 4; center = 5; bottom = 6 }
  $offset = [double]$offsets[$Preset]
  $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
  $tab = Get-Fact $tabHwnd
  if ($null -eq $tab) {
    [void]$errors.Add("native-position-preset: Hover Tab was not visible for '$Preset'")
  } else {
    $center = Center-Fact $tab
    if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) {
      [void]$errors.Add("native-position-preset: could not open the native menu for '$Preset'")
    } else {
      try {
        [void](Wait-ForNativeMenu "position '$Preset' native menu")
        Select-NativeMenuEntry ([int]$downCounts[$Preset])
        Start-Sleep -Milliseconds 120
        [void](Wait-ForSharePreference { param($value) [Math]::Abs([double]$value.hoverTabVerticalOffset - $offset) -lt 0.001 } "hover-tab '$Preset' position")
      } catch {
        [void]$errors.Add("native-position-preset: $($_.Exception.Message)")
      }
    }
  }
  $preferences = Read-SharePreferences
  $updatedTarget = Get-Fact $TargetHwnd
  $updatedTab = Get-Fact $tabHwnd
  $workArea = Get-WorkingArea $TargetHwnd
  if ($null -ne $updatedTarget -and $null -ne $updatedTab) {
    $geometryError = Get-PositionGeometryError $updatedTarget $updatedTab $offset $workArea
    $tolerance = [Math]::Max(3, [int][Math]::Ceiling(3 * $updatedTab.dpi / 96.0))
    if ($geometryError -gt $tolerance) {
      [void]$errors.Add("native-position-preset: '$Preset' geometry error ${geometryError}px exceeded ${tolerance}px")
    }
    $workAreaErrors = @(Check-WorkAreaContainment $updatedTab (Get-WorkingArea $TargetHwnd) "native-position-preset '$Preset'")
    foreach ($errorText in $workAreaErrors) { [void]$errors.Add([string]$errorText) }
  } else {
    [void]$errors.Add("native-position-preset: target or tab disappeared after '$Preset'")
  }
  Add-Observation "native-position-preset-$Preset" $updatedTarget $updatedTab @{
    selected = $Preset
    expectedOffset = $offset
    persistedOffset = if ($null -eq $preferences) { $null } else { $preferences.hoverTabVerticalOffset }
    preferencePath = Get-SharePreferencePath
    geometryErrorPx = if ($null -eq $updatedTarget -or $null -eq $updatedTab) { $null } else { [math]::Round((Get-PositionGeometryError $updatedTarget $updatedTab $offset $workArea), 1) }
    errors = @($errors)
  }
  return [pscustomobject]@{ Target = $updatedTarget; Tab = $updatedTab; Preferences = $preferences; Errors = @($errors) }
}

function Get-ShareLogPath() {
  if ($env:APPDATA) { return Join-Path $env:APPDATA 'Petal\logs\petal.log' }
  return Join-Path ([IO.Path]::GetTempPath()) 'Petal\logs\petal.log'
}

function Get-ShareMarkers() {
  $logPath = Get-ShareLogPath
  if (-not (Test-Path -LiteralPath $logPath)) { return @() }
  return @(Get-Content -LiteralPath $logPath -Tail 240 | Select-String -Pattern 'direct (Share|Stop) (action|completed)|toggle_window_share|toggle_share_for_window|START sharing|STOP sharing|share-state-changed|share-priority: saved|draw request applied|share control mode changed')
}

function Wait-ForShareMarkers([int]$Before, [string]$Description) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $markers = @(Get-ShareMarkers)
    if ($markers.Count -gt $Before) { return $markers }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "$Description did not advance beyond marker count $Before within $TimeoutSeconds seconds."
}

function Wait-ForShareLogPattern([string]$Pattern, [string]$Description) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $matches = @(Get-ShareMarkers | Where-Object { $_.Line -match $Pattern })
    if ($matches.Count -gt 0) { return $matches }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "$Description was not observed within $TimeoutSeconds seconds."
}

function Invoke-ActiveShareMenuActions($Target, [IntPtr]$TargetHwnd) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $drawOn = $false
  $drawOff = $false
  $controlFull = $false
  $controlRestored = $false
  $shared = $false
  $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
  $tab = Get-Fact $tabHwnd

  try {
    if ($null -eq $tab) { throw 'Hover Tab was not visible before active-action exercise.' }
    $center = Center-Fact $tab
    $before = @(Get-ShareMarkers)
    if (-not [PetalHoverTabSmoke]::ClickAt($center[0], $center[1])) {
      throw 'direct Share SendInput failed.'
    }
    $markers = @(Wait-ForShareMarkers $before.Count 'active-action direct Share')
    $shared = @($markers | Where-Object { $_.Line -match 'direct Share completed.*shared=true' }).Count -gt 0
    if (-not $shared) { throw 'direct Share completion marker was not observed.' }
    Start-Sleep -Milliseconds 350
  } catch {
    [void]$errors.Add("active-share-actions: $($_.Exception.Message)")
  }

  if ($shared) {
    try {
      $tab = Get-Fact $tabHwnd
      $center = Center-Fact $tab
      if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) { throw 'could not open Draw menu' }
      [void](Wait-ForNativeMenu 'active Draw menu')
      Select-NativeMenuEntry 10 # priorities 0-3, positions 4-6, modes 7-8, Debug 9, Draw 10
      [void](Wait-ForShareLogPattern 'draw request applied.*active=(true|True)' 'Draw-on callback')
      $drawOn = $true
    } catch {
      [void]$errors.Add("active-share-actions: Draw-on failed: $($_.Exception.Message)")
    }

    try {
      $tab = Get-Fact $tabHwnd
      $center = Center-Fact $tab
      if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) { throw 'could not reopen Draw menu' }
      [void](Wait-ForNativeMenu 'active Draw-off menu')
      Select-NativeMenuEntry 10
      [void](Wait-ForShareLogPattern 'draw request applied.*active=(false|False)' 'Draw-off callback')
      $drawOff = $true
    } catch {
      [void]$errors.Add("active-share-actions: Draw-off failed: $($_.Exception.Message)")
    }

    try {
      $tab = Get-Fact $tabHwnd
      $center = Center-Fact $tab
      if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) { throw 'could not open control-mode menu' }
      [void](Wait-ForNativeMenu 'active full-control menu')
      Select-NativeMenuEntry 8
      [void](Wait-ForShareLogPattern 'share control mode changed.*mode=fullControl' 'Full-control callback')
      $controlFull = $true

      $tab = Get-Fact $tabHwnd
      $center = Center-Fact $tab
      if (-not [PetalHoverTabSmoke]::RightClickAt($center[0], $center[1])) { throw 'could not reopen control-mode menu' }
      [void](Wait-ForNativeMenu 'active cursor-preserving menu')
      Select-NativeMenuEntry 7
      [void](Wait-ForShareLogPattern 'share control mode changed.*mode=cursorPreserving' 'Cursor-preserving callback')
      $controlRestored = $true
    } catch {
      [void]$errors.Add("active-share-actions: control-mode failed: $($_.Exception.Message)")
    }

    try {
      $tab = Get-Fact $tabHwnd
      $center = Center-Fact $tab
      $before = @(Get-ShareMarkers)
      if (-not [PetalHoverTabSmoke]::ClickAt($center[0], $center[1])) { throw 'direct Stop SendInput failed.' }
      $markers = @(Wait-ForShareMarkers $before.Count 'active-action direct Stop')
      if (-not @($markers | Where-Object { $_.Line -match 'direct Stop completed.*shared=false' }).Count) {
        throw 'direct Stop completion marker was not observed.'
      }
    } catch {
      [void]$errors.Add("active-share-actions: Stop failed: $($_.Exception.Message)")
    }
  }

  $tab = Get-Fact $tabHwnd
  Add-Observation 'active-share-menu-actions' $Target $tab @{
    shareStarted = $shared
    drawOn = $drawOn
    drawOff = $drawOff
    controlFull = $controlFull
    controlRestored = $controlRestored
    enabledActionsVerified = ($errors.Count -eq 0)
    errors = @($errors)
  }
  return [pscustomobject]@{
    Target = $Target
    Tab = $tab
    Errors = @($errors)
  }
}

function Get-FrameEdgeError($Actual, $Expected) {
  if ($null -eq $Actual -or $null -eq $Expected) { return [double]::PositiveInfinity }
  $horizontal = [Math]::Max(
    [Math]::Abs($Actual.x - $Expected.x),
    [Math]::Abs(($Actual.x + $Actual.width) - ($Expected.x + $Expected.width)))
  $vertical = [Math]::Max(
    [Math]::Abs($Actual.y - $Expected.y),
    [Math]::Abs(($Actual.y + $Actual.height) - ($Expected.y + $Expected.height)))
  $size = [Math]::Max(
    [Math]::Abs($Actual.width - $Expected.width),
    [Math]::Abs($Actual.height - $Expected.height))
  return [double][Math]::Max($horizontal, [Math]::Max($vertical, $size))
}

function Get-TabEdgeError($Target, $Tab) {
  if ($null -eq $Target -or $null -eq $Tab) { return [double]::PositiveInfinity }
  $targetRight = $Target.x + $Target.width
  $expectedSize = Expected-Pixels 40 $Tab
  $expectedCenterY = $Target.y + [int]($Target.height / 2)
  $actualCenterY = $Tab.y + [int]($Tab.height / 2)
  $vertical = [Math]::Abs($actualCenterY - $expectedCenterY)
  $size = [Math]::Max(
    [Math]::Abs($Tab.width - $expectedSize),
    [Math]::Abs($Tab.height - $expectedSize))
  $outside = [Math]::Max(
    [Math]::Abs($Tab.x - $targetRight),
    [Math]::Max($vertical, $size))
  $inset = [Math]::Max(
    [Math]::Abs(($Tab.x + $Tab.width) - $targetRight),
    [Math]::Max($vertical, $size))
  return [double][Math]::Min($outside, $inset)
}

function Get-FollowSnapshot([IntPtr]$TargetHwnd) {
  $target = Get-Fact $TargetHwnd
  $border = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Petal Sharer Pointer'))
  $tab = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
  return [pscustomobject]@{
    target = $target
    border = $border
    tab = $tab
  }
}

function Add-FollowSample(
  [string]$Action,
  [double]$ElapsedMs,
  $Snapshot,
  $PreviousTarget
) {
  $target = $Snapshot.target
  $border = $Snapshot.border
  $tab = $Snapshot.tab
  $targetVisible = $null -ne $target -and $target.visible -and -not $target.minimized
  $borderVisible = $null -ne $border -and $border.visible -and -not $border.minimized
  $tabVisible = $null -ne $tab -and $tab.visible -and -not $tab.minimized
  $borderError = Get-FrameEdgeError $border $target
  $tabError = Get-TabEdgeError $target $tab
  $tabPreviousError = Get-TabEdgeError $PreviousTarget $tab
  $borderCurrent = $targetVisible -and $borderVisible -and $borderError -le 2
  $tabCurrent = $targetVisible -and $tabVisible -and $tabError -le 2
  $tabPrevious = $null -ne $PreviousTarget -and $tabVisible -and $tabPreviousError -le 2
  $sample = [pscustomobject]@{
    atMs = [math]::Round($ElapsedMs, 1)
    action = $Action
    targetVisible = $targetVisible
    borderVisible = $borderVisible
    tabVisible = $tabVisible
    visibilityGap = $targetVisible -and (-not $borderVisible -or -not $tabVisible)
    borderCurrent = $borderCurrent
    tabCurrent = $tabCurrent
    tabPrevious = $tabPrevious
    borderCurrentTabPrevious = $borderCurrent -and $tabPrevious
    borderEdgeErrorPx = if ([double]::IsInfinity($borderError)) { $null } else { [math]::Round($borderError, 1) }
    tabEdgeErrorPx = if ([double]::IsInfinity($tabError)) { $null } else { [math]::Round($tabError, 1) }
    tabPreviousEdgeErrorPx = if ([double]::IsInfinity($tabPreviousError)) { $null } else { [math]::Round($tabPreviousError, 1) }
    targetX = if ($null -eq $target) { $null } else { $target.x }
    targetY = if ($null -eq $target) { $null } else { $target.y }
    targetWidth = if ($null -eq $target) { $null } else { $target.width }
    targetHeight = if ($null -eq $target) { $null } else { $target.height }
    borderX = if ($null -eq $border) { $null } else { $border.x }
    borderY = if ($null -eq $border) { $null } else { $border.y }
    borderWidth = if ($null -eq $border) { $null } else { $border.width }
    borderHeight = if ($null -eq $border) { $null } else { $border.height }
    tabX = if ($null -eq $tab) { $null } else { $tab.x }
    tabY = if ($null -eq $tab) { $null } else { $tab.y }
    tabWidth = if ($null -eq $tab) { $null } else { $tab.width }
    tabHeight = if ($null -eq $tab) { $null } else { $tab.height }
  }
  [void]$followSamples.Add($sample)
  return $sample
}

function Invoke-FollowPositiveControl($Target, [IntPtr]$TargetHwnd) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
  $tab = Get-Fact $tabHwnd
  if ($null -eq $tab) {
    [void]$errors.Add('continuous-follow-positive-control: Hover Tab is not visible')
    Add-Observation 'continuous-follow-positive-control' $Target $null @{
      detectorWentRed = $false
      restoredAfterSourceMove = $false
      errors = @($errors)
    }
    return [pscustomobject]@{ Target = $Target; Tab = $null; Errors = @($errors) }
  }

  $originalTab = $tab
  $offsetPixels = Expected-Pixels 80 $tab
  if (-not [PetalHoverTabSmoke]::SetWindowFrame(
      $tabHwnd,
      $tab.x - $offsetPixels,
      $tab.y,
      $tab.width,
      $tab.height)) {
    [void]$errors.Add('continuous-follow-positive-control: could not offset the owned Hover Tab')
  }
  Start-Sleep -Milliseconds 20
  $offsetTab = Get-Fact $tabHwnd
  $offsetError = Get-TabEdgeError $Target $offsetTab
  $detectorWentRed = $offsetError -gt 2
  if (-not $detectorWentRed) {
    [void]$errors.Add("continuous-follow-positive-control: offset detector stayed green (edge error $offsetError px)")
  }

  $sourceBeforeMove = Get-Fact $TargetHwnd
  $restoredTarget = $sourceBeforeMove
  $restoredTab = $offsetTab
  $restoredAfterSourceMove = $false
  if ($null -eq $sourceBeforeMove) {
    [void]$errors.Add('continuous-follow-positive-control: source disappeared before restoration move')
  } elseif (-not [PetalHoverTabSmoke]::SetWindowFrame(
      $TargetHwnd,
      $sourceBeforeMove.x + 6,
      $sourceBeforeMove.y + 4,
      $sourceBeforeMove.width,
      $sourceBeforeMove.height)) {
    [void]$errors.Add('continuous-follow-positive-control: could not move the private source window')
  } else {
    $movedTargetCenter = @(
      [int]($sourceBeforeMove.x + 6 + $sourceBeforeMove.width / 2),
      [int]($sourceBeforeMove.y + 4 + $sourceBeforeMove.height / 2))
    [void][PetalHoverTabSmoke]::MoveCursor($movedTargetCenter[0], $movedTargetCenter[1])
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
      $restoredTarget = Get-Fact $TargetHwnd
      $restoredTab = Get-Fact $tabHwnd
      if ($null -ne $restoredTarget -and $null -ne $restoredTab -and
          (Get-TabEdgeError $restoredTarget $restoredTab) -le 2) {
        $restoredAfterSourceMove = $true
        break
      }
      Start-Sleep -Milliseconds 8
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $restoredAfterSourceMove) {
      [void]$errors.Add('continuous-follow-positive-control: source move did not restore the offset tab')
      [void][PetalHoverTabSmoke]::SetWindowFrame(
        $tabHwnd,
        $originalTab.x,
        $originalTab.y,
        $originalTab.width,
        $originalTab.height)
      $restoredTarget = Get-Fact $TargetHwnd
      $restoredTab = Get-Fact $tabHwnd
    }
  }

  Add-Observation 'continuous-follow-positive-control' $Target $offsetTab @{
    detectorWentRed = $detectorWentRed
    offsetEdgeErrorPx = if ([double]::IsInfinity($offsetError)) { $null } else { [math]::Round($offsetError, 1) }
    restoredAfterSourceMove = $restoredAfterSourceMove
    restorationEdgeErrorPx = if ($null -eq $restoredTarget -or $null -eq $restoredTab) { $null } else { [math]::Round((Get-TabEdgeError $restoredTarget $restoredTab), 1) }
    errors = @($errors)
  }
  return [pscustomobject]@{
    Target = if ($null -eq $restoredTarget) { $Target } else { $restoredTarget }
    Tab = $restoredTab
    Errors = @($errors)
  }
}

function Invoke-ContinuousFollow($Target, [IntPtr]$TargetHwnd, [string]$Attachment, [string]$Label) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $initial = Get-Fact $TargetHwnd
  $borderHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Petal Sharer Pointer')
  $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
  if ($null -eq $initial -or $borderHwnd -eq [IntPtr]::Zero -or $tabHwnd -eq [IntPtr]::Zero) {
    [void]$errors.Add("${Label}: source, Petal border, or Hover Tab was not visible for continuous follow")
    Add-Observation "${Label}-continuous-follow" $Target $null @{
      attachment = $Attachment
      sampleCount = 0
      visibilityGapSamples = 0
      borderCurrentTabPreviousSamples = 0
      errors = @($errors)
    }
    return [pscustomobject]@{ Target = $Target; Tab = $null; Errors = @($errors) }
  }

  $previousTarget = $initial
  $startedAt = [System.Diagnostics.Stopwatch]::StartNew()
  for ($step = 1; $step -le 60; $step++) {
    $x = $initial.x + $step * 4
    $y = $initial.y + $step * 2
    if (-not [PetalHoverTabSmoke]::SetWindowFrame(
        $TargetHwnd, $x, $y, $initial.width, $initial.height)) {
      [void]$errors.Add("${Label}: failed to move source at step $step")
      break
    }
    [void][PetalHoverTabSmoke]::MoveCursor(
      [int]($x + $initial.width / 2),
      [int]($y + $initial.height / 2))
    Start-Sleep -Milliseconds 8
    $snapshot = Get-FollowSnapshot $TargetHwnd
    $sample = Add-FollowSample $Label $startedAt.Elapsed.TotalMilliseconds $snapshot $previousTarget
    if ($null -ne $snapshot.target) { $previousTarget = $snapshot.target }
  }

  # Restore the private source so subsequent lifecycle/menu checks start from
  # the same window geometry, then sample until both surfaces settle together.
  [void][PetalHoverTabSmoke]::SetWindowFrame(
    $TargetHwnd,
    $initial.x,
    $initial.y,
    $initial.width,
    $initial.height)
  [void][PetalHoverTabSmoke]::MoveCursor(
    [int]($initial.x + $initial.width / 2),
    [int]($initial.y + $initial.height / 2))
  $settledTarget = $initial
  $settledTab = $null
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $snapshot = Get-FollowSnapshot $TargetHwnd
    $settledTarget = $snapshot.target
    $settledTab = $snapshot.tab
    if ($null -ne $settledTarget -and $null -ne $settledTab -and
        (Get-FrameEdgeError $snapshot.border $settledTarget) -le 2 -and
        (Get-TabEdgeError $settledTarget $settledTab) -le 2) {
      break
    }
    Start-Sleep -Milliseconds 8
  } while ([DateTime]::UtcNow -lt $deadline)
  if ($null -eq $settledTarget -or $null -eq $settledTab -or
      (Get-FrameEdgeError $snapshot.border $settledTarget) -gt 2 -or
      (Get-TabEdgeError $settledTarget $settledTab) -gt 2) {
    [void]$errors.Add("${Label}: source, border, and tab did not settle after continuous movement")
  }

  $samplesForLabel = @($followSamples | Where-Object { $_.action -eq $Label })
  $visibilityGaps = @($samplesForLabel | Where-Object { $_.visibilityGap }).Count
  $borderCurrentTabPrevious = @($samplesForLabel | Where-Object { $_.borderCurrentTabPrevious }).Count
  $tabErrors = @($samplesForLabel | Where-Object { $null -ne $_.tabEdgeErrorPx } | ForEach-Object { [double]$_.tabEdgeErrorPx })
  $borderErrors = @($samplesForLabel | Where-Object { $null -ne $_.borderEdgeErrorPx } | ForEach-Object { [double]$_.borderEdgeErrorPx })
  $maxTabError = if ($tabErrors.Count -eq 0) { $null } else { [math]::Round(([double]($tabErrors | Measure-Object -Maximum).Maximum), 1) }
  $maxBorderError = if ($borderErrors.Count -eq 0) { $null } else { [math]::Round(([double]($borderErrors | Measure-Object -Maximum).Maximum), 1) }
  if ($visibilityGaps -gt 0) { [void]$errors.Add("${Label}: $visibilityGaps visibility-gap samples") }
  if ($borderCurrentTabPrevious -gt 0) {
    [void]$errors.Add("${Label}: $borderCurrentTabPrevious border-current/tab-previous samples")
  }
  if ($null -ne $settledTarget -and $null -ne $settledTab) {
    $settledTabError = Get-TabEdgeError $settledTarget $settledTab
    if ($settledTabError -gt 2) { [void]$errors.Add("${Label}: settled tab edge error was $settledTabError px") }
  } else {
    $settledTabError = [double]::PositiveInfinity
  }
  Add-Observation "${Label}-continuous-follow" $settledTarget $settledTab @{
    attachment = $Attachment
    sampleCount = $samplesForLabel.Count
    visibilityGapSamples = $visibilityGaps
    borderCurrentTabPreviousSamples = $borderCurrentTabPrevious
    maxTabEdgeErrorPx = $maxTabError
    maxBorderEdgeErrorPx = $maxBorderError
    settledTabEdgeErrorPx = if ([double]::IsInfinity($settledTabError)) { $null } else { [math]::Round($settledTabError, 1) }
    errors = @($errors)
  }
  return [pscustomobject]@{ Target = $settledTarget; Tab = $settledTab; Errors = @($errors) }
}

function Invoke-TaskbarEdgePlacement([IntPtr]$TargetHwnd, [bool]$ExerciseShare) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $workArea = Get-WorkingArea $TargetHwnd
  $original = Get-Fact $TargetHwnd
  $edgeTarget = $null
  $edgeTab = $null
  $edgeTabHwnd = [IntPtr]::Zero
  $cursorTransferPassed = $false
  $shareStopPassed = $false
  $shareClickSent = $false
  $stopClickSent = $false

  try {
    if ($null -eq $workArea) {
      [void]$errors.Add('taskbar-edge-placement: Screen.WorkingArea was unavailable')
    } elseif ($null -eq $original) {
      [void]$errors.Add('taskbar-edge-placement: sacrificial source was unavailable')
    } else {
      # Put the source center on the actual bottom work-area boundary. A
      # bottom taskbar then makes the old full-monitor projection overlap the
      # reserved strip, while the fixed tab must clamp wholly inside rcWork.
      $edgeHeight = [Math]::Max(40, [Math]::Min([int]$original.height, 180))
      $edgeWidth = [int]$original.width
      $edgeX = [int][Math]::Max(
        [int]$workArea.Left,
        [Math]::Min([int]$original.x, [int]$workArea.Right - $edgeWidth))
      $edgeY = [int]($workArea.Bottom - [Math]::Floor($edgeHeight / 2.0))
      if (-not [PetalHoverTabSmoke]::SetWindowFrame(
          $TargetHwnd, $edgeX, $edgeY, $edgeWidth, $edgeHeight)) {
        [void]$errors.Add('taskbar-edge-placement: could not move the private source to the work-area boundary')
      } else {
        [void][PetalHoverTabSmoke]::MoveCursor(
          [int]($edgeX + $edgeWidth / 2),
          [int]($edgeY + $edgeHeight / 2))
        Start-Sleep -Milliseconds 120
        try {
          $edgeTarget = Wait-ForFact { Get-Fact $TargetHwnd } 'Taskbar-edge source window'
        } catch {
          [void]$errors.Add("taskbar-edge-placement: $($_.Exception.Message)")
        }
        if ($null -ne $edgeTarget) {
          $edgeTabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
          try {
            $edgeTab = Wait-ForWorkAreaTab $workArea 'Taskbar-edge Hover Tab'
          } catch {
            [void]$errors.Add("taskbar-edge-placement: $($_.Exception.Message)")
          }
          $edgeTabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
          if ($null -ne $edgeTab) {
            $edgeErrors = @(Check-WorkAreaContainment $edgeTab $workArea 'taskbar-edge placement')
            foreach ($errorText in $edgeErrors) { [void]$errors.Add([string]$errorText) }
            $tabCenter = Center-Fact $edgeTab
            if (-not [PetalHoverTabSmoke]::MoveCursor($tabCenter[0], $tabCenter[1])) {
              [void]$errors.Add('taskbar-edge-placement: cursor transfer to the tab failed')
            } else {
              Start-Sleep -Milliseconds 120
              $transferredTab = Get-Fact $edgeTabHwnd
              $transferErrors = @(Check-WorkAreaContainment $transferredTab $workArea 'taskbar-edge cursor transfer')
              foreach ($errorText in $transferErrors) { [void]$errors.Add([string]$errorText) }
              $cursorTransferPassed = $transferErrors.Count -eq 0
              if ($cursorTransferPassed) { $edgeTab = $transferredTab }
            }

            if ($ExerciseShare -and $cursorTransferPassed) {
              $beforeShareMarkers = @(Get-ShareMarkers)
              $shareClickSent = [PetalHoverTabSmoke]::ClickAt($tabCenter[0], $tabCenter[1])
              if (-not $shareClickSent) {
                [void]$errors.Add('taskbar-edge-placement: direct Share click failed')
              }
              $shareMarkers = @()
              if ($shareClickSent) {
                try {
                  $shareMarkers = @(Wait-ForShareMarkers $beforeShareMarkers.Count 'taskbar-edge direct Share')
                } catch {
                  [void]$errors.Add("taskbar-edge-placement: $($_.Exception.Message)")
                  $shareMarkers = @(Get-ShareMarkers)
                }
                if (-not @($shareMarkers | Where-Object { $_.Line -match 'direct Share completed.*shared=true' }).Count) {
                  [void]$errors.Add('taskbar-edge-placement: direct Share completion marker was not observed')
                }
                Start-Sleep -Milliseconds 350
                $afterShareTab = Get-Fact $edgeTabHwnd
                $afterShareErrors = @(Check-WorkAreaContainment $afterShareTab $workArea 'taskbar-edge after Share')
                foreach ($errorText in $afterShareErrors) { [void]$errors.Add([string]$errorText) }
                if ($null -ne $afterShareTab) { $edgeTab = $afterShareTab }

                $stopTab = if ($null -ne $afterShareTab) { $afterShareTab } else { $edgeTab }
                if ($null -ne $stopTab) {
                  $stopCenter = Center-Fact $stopTab
                  $beforeStopMarkers = @(Get-ShareMarkers)
                  $stopClickSent = [PetalHoverTabSmoke]::ClickAt($stopCenter[0], $stopCenter[1])
                  if (-not $stopClickSent) {
                    [void]$errors.Add('taskbar-edge-placement: direct Stop click failed')
                  } else {
                    try {
                      $stopMarkers = @(Wait-ForShareMarkers $beforeStopMarkers.Count 'taskbar-edge direct Stop')
                    } catch {
                      [void]$errors.Add("taskbar-edge-placement: $($_.Exception.Message)")
                      $stopMarkers = @(Get-ShareMarkers)
                    }
                    if (-not @($stopMarkers | Where-Object { $_.Line -match 'direct Stop completed.*shared=false' }).Count) {
                      [void]$errors.Add('taskbar-edge-placement: direct Stop completion marker was not observed')
                    }
                    Start-Sleep -Milliseconds 350
                    $afterStopTab = Get-Fact $edgeTabHwnd
                    $afterStopErrors = @(Check-WorkAreaContainment $afterStopTab $workArea 'taskbar-edge after Stop')
                    foreach ($errorText in $afterStopErrors) { [void]$errors.Add([string]$errorText) }
                    if ($null -ne $afterStopTab) { $edgeTab = $afterStopTab }
                    $shareStopPassed = $afterStopErrors.Count -eq 0
                  }
                } else {
                  [void]$errors.Add('taskbar-edge-placement: Hover Tab disappeared before direct Stop')
                }
              }
            }
          }
        }
      }
    }
  } catch {
    [void]$errors.Add("taskbar-edge-placement: unexpected error: $($_.Exception.Message)")
  } finally {
    if ($null -ne $original) {
      [void][PetalHoverTabSmoke]::SetWindowFrame(
        $TargetHwnd, $original.x, $original.y, $original.width, $original.height)
      [void][PetalHoverTabSmoke]::MoveCursor((Center-Fact $original)[0], (Center-Fact $original)[1])
      Start-Sleep -Milliseconds 120
    }
  }

  $restoredTarget = Get-Fact $TargetHwnd
  $restoredTab = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
  Add-Observation 'taskbar-edge-placement' $edgeTarget $edgeTab @{
    workingArea = if ($null -eq $workArea) { $null } else { [pscustomobject]@{ left = $workArea.Left; top = $workArea.Top; right = $workArea.Right; bottom = $workArea.Bottom } }
    sourceCenteredAtWorkAreaEdge = ($null -ne $edgeTarget -and $null -ne $workArea)
    cursorTransfer = $cursorTransferPassed
    directShareStop = $shareStopPassed
    shareClickSent = $shareClickSent
    stopClickSent = $stopClickSent
    restored = ($null -ne $restoredTarget)
    errors = @($errors)
  }
  return [pscustomobject]@{
    Target = $restoredTarget
    Tab = $restoredTab
    Errors = @($errors)
  }
}

function Invoke-OcclusionExercise([IntPtr]$TargetHwnd) {
  $errors = [System.Collections.Generic.List[string]]::new()
  $targetBefore = Get-Fact $TargetHwnd
  $sourceCenter = $null
  $tabHwnd = [IntPtr]::Zero
  $tabBefore = $null
  $tabCovered = $null
  $occluderHwnd = [IntPtr]::Zero
  $occluderFact = $null
  $occluderAboveTab = $false
  $tabAboveSource = $false
  $occluderOwnsTabPoint = $false
  $cursorStayedOnSource = $false

  try {
    if ($null -eq $targetBefore) { throw 'source was unavailable before occlusion exercise' }
    $sourceCenter = Center-Fact $targetBefore
    if (-not [PetalHoverTabSmoke]::MoveCursor($sourceCenter[0], $sourceCenter[1])) {
      throw 'could not place cursor on the visible source region'
    }
    Start-Sleep -Milliseconds 180
    $targetBefore = Get-Fact $TargetHwnd
    $tabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
    $tabBefore = Get-Fact $tabHwnd
    if ($null -eq $tabBefore) { throw 'Hover Tab was not visible before placing the occluder' }

    $occluderHwnd = [PetalHoverTabSmoke]::StartOccluderWindow()
    if ($occluderHwnd -eq [IntPtr]::Zero) { throw 'could not create the private occluder window' }
    $coverSize = [Math]::Max(64, [int]($tabBefore.width + 16))
    $coverX = [int]($tabBefore.x - [Math]::Floor(($coverSize - $tabBefore.width) / 2.0))
    $coverY = [int]($tabBefore.y - [Math]::Floor(($coverSize - $tabBefore.height) / 2.0))
    if (-not [PetalHoverTabSmoke]::PlaceAboveNoActivate(
        $occluderHwnd,
        $tabHwnd,
        $coverX,
        $coverY,
        $coverSize,
        $coverSize,
        $true)) {
      throw 'could not place the private occluder above Hover Tab'
    }
    Start-Sleep -Milliseconds 160

    $occluderFact = Get-Fact $occluderHwnd
    $tabCovered = Get-Fact $tabHwnd
    $tabCenter = Center-Fact $tabCovered
    $occluderAboveTab = [PetalHoverTabSmoke]::IsAboveInZOrder($occluderHwnd, $tabHwnd)
    $tabAboveSource = [PetalHoverTabSmoke]::IsAboveInZOrder($tabHwnd, $TargetHwnd)
    $occluderOwnsTabPoint = $null -ne $occluderFact -and $occluderAboveTab -and
      $tabCenter[0] -ge $occluderFact.x -and
      $tabCenter[0] -lt $occluderFact.x + $occluderFact.width -and
      $tabCenter[1] -ge $occluderFact.y -and
      $tabCenter[1] -lt $occluderFact.y + $occluderFact.height
    $cursorStayedOnSource = [PetalHoverTabSmoke]::TopmostAtCursor() -eq $TargetHwnd
    if ($null -eq $occluderFact -or -not $occluderFact.visible) {
      [void]$errors.Add('hover-tab-occlusion: private occluder was not visible')
    }
    if ($null -eq $tabCovered -or -not $tabCovered.visible) {
      [void]$errors.Add('hover-tab-occlusion: Hover Tab native window became invisible')
    }
    if (-not $occluderAboveTab) {
      [void]$errors.Add('hover-tab-occlusion: occluder was not above Hover Tab in native z-order')
    }
    if (-not $tabAboveSource) {
      [void]$errors.Add('hover-tab-occlusion: Hover Tab was not above its source in native z-order')
    }
    if (-not $occluderOwnsTabPoint) {
      [void]$errors.Add('hover-tab-occlusion: tab point was not owned by the private occluder')
    }
    if (-not $cursorStayedOnSource) {
      [void]$errors.Add('hover-tab-occlusion: cursor left the visible source region')
    }
  } catch {
    [void]$errors.Add("hover-tab-occlusion: unexpected error: $($_.Exception.Message)")
  } finally {
    if ($occluderHwnd -ne [IntPtr]::Zero) {
      [void][PetalHoverTabSmoke]::HideNoActivate($occluderHwnd)
      [PetalHoverTabSmoke]::StopOccluderWindow()
      if (-not [PetalHoverTabSmoke]::WaitForOccluderStopped(5000)) {
        [void]$errors.Add('hover-tab-occlusion: private occluder thread did not terminate')
      }
    }
    if ($null -ne $sourceCenter) {
      [void][PetalHoverTabSmoke]::MoveCursor($sourceCenter[0], $sourceCenter[1])
      Start-Sleep -Milliseconds 150
    }
  }

  $restoredTarget = Get-Fact $TargetHwnd
  $restoredTabHwnd = [IntPtr]::Zero
  $restoredTab = $null
  if ($null -ne $restoredTarget) {
    try {
      $restoredTabHwnd = Wait-ForHwnd { [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab') } 'Hover Tab after occluder cleanup'
      $restoredTab = Get-Fact $restoredTabHwnd
    } catch {
      [void]$errors.Add("hover-tab-occlusion: $($_.Exception.Message)")
    }
  }
  $restoredAdjacent = $null -ne $restoredTab -and
    [PetalHoverTabSmoke]::IsAboveInZOrder($restoredTabHwnd, $TargetHwnd)
  if ($null -ne $restoredTarget -and -not $restoredAdjacent) {
    [void]$errors.Add('hover-tab-occlusion: source-relative tab order was not restored after cleanup')
  }

  Add-Observation 'hover-tab-occlusion' $targetBefore $tabCovered @{
    occluder = $occluderFact
    occluderAboveTab = $occluderAboveTab
    tabAboveSource = $tabAboveSource
    occluderOwnsTabPoint = $occluderOwnsTabPoint
    cursorStayedOnSource = $cursorStayedOnSource
    restored = ($null -ne $restoredTarget -and $null -ne $restoredTab)
    restoredAdjacent = $restoredAdjacent
    errors = @($errors)
  }
  return [pscustomobject]@{
    Target = $restoredTarget
    Tab = $restoredTab
    Errors = @($errors)
  }
}

function Invoke-ShareStopReuse($Target, [string]$Attachment, [string]$Label) {
  $sequenceErrors = [System.Collections.Generic.List[string]]::new()
  $tab = $null
  try {
    $tab = Wait-ForUnifiedTab $Target "$Label initial Hover Tab"
  } catch {
    [void]$sequenceErrors.Add("${Label}: $($_.Exception.Message)")
  }

  if ($null -eq $tab) {
    Add-Observation "${Label}-share-stop-reuse" $Target $null @{
      attachment = $Attachment
      reused = $false
      fixed40x40 = $false
      actionCount = 0
      errors = @($sequenceErrors)
    }
    foreach ($errorText in $sequenceErrors) { [void]$failures.Add([string]$errorText) }
    return $null
  }

  $actions = @(
    [pscustomobject]@{ Name = 'Share'; ActionMarker = 'direct Share action'; CompletionPattern = 'direct Share completed.*shared=true' },
    [pscustomobject]@{ Name = 'Stop'; ActionMarker = 'direct Stop action'; CompletionPattern = 'direct Stop completed.*shared=false' },
    [pscustomobject]@{ Name = 'Share'; ActionMarker = 'direct Share action'; CompletionPattern = 'direct Share completed.*shared=true' },
    [pscustomobject]@{ Name = 'Stop'; ActionMarker = 'direct Stop action'; CompletionPattern = 'direct Stop completed.*shared=false' }
  )
  $lastTab = $tab
  $completedActions = 0

  foreach ($action in $actions) {
    if ($completedActions -gt 0) {
      try {
        $tab = Wait-ForUnifiedTab $Target "$Label before direct $($action.Name)"
      } catch {
        [void]$sequenceErrors.Add("${Label}: $($_.Exception.Message)")
        break
      }
    }

    $center = Center-Fact $tab
    $beforeMarkers = @(Get-ShareMarkers)
    if (-not [PetalHoverTabSmoke]::ClickAt($center[0], $center[1])) {
      [void]$sequenceErrors.Add("${Label}: direct $($action.Name) SendInput failed")
    }
    $markers = @()
    try {
      $markers = @(Wait-ForShareMarkers $beforeMarkers.Count "$Label direct $($action.Name) lifecycle marker")
    } catch {
      [void]$sequenceErrors.Add("${Label}: $($_.Exception.Message)")
      $markers = @(Get-ShareMarkers)
    }
    Start-Sleep -Milliseconds 350

    $followResult = $null
    if ($ExerciseFollow -and $completedActions -eq 0 -and $action.Name -eq 'Share') {
      $followResult = Invoke-ContinuousFollow $Target $targetHwnd $Attachment $Label
      foreach ($followError in $followResult.Errors) {
        [void]$sequenceErrors.Add([string]$followError)
      }
    }
    $after = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
    $actionErrors = @(Check-UnifiedGeometry $Target $after "${Label} after direct $($action.Name)")
    if ($null -ne $followResult) {
      $actionErrors += @($followResult.Errors)
    }
    if ($null -ne $after) {
      $actionErrors += @(Check-TabAttachment $Target $after $Attachment "${Label} after direct $($action.Name)")
    }
    if ($markers.Count -le $beforeMarkers.Count) {
      $actionErrors += "${Label}: direct $($action.Name) marker count did not advance"
    }
    if (-not @($markers | Where-Object { $_.Line -match [string]$action.ActionMarker }).Count) {
      $actionErrors += "${Label}: $($action.ActionMarker) marker was not observed"
    }
    if (-not @($markers | Where-Object { $_.Line -match [string]$action.CompletionPattern }).Count) {
      $actionErrors += "${Label}: $($action.CompletionPattern) marker was not observed"
    }

    $observationName = "${Label}-$($action.Name.ToLowerInvariant())-$($completedActions + 1)"
    Add-Observation $observationName $Target $after @{
      action = $action.Name
      attachment = $Attachment
      markerAdvanced = ($markers.Count -gt $beforeMarkers.Count)
      fixed40x40 = ($actionErrors.Count -eq 0)
      errors = $actionErrors
      markerCount = $markers.Count
    }
    foreach ($errorText in $actionErrors) {
      [void]$sequenceErrors.Add([string]$errorText)
    }
    $lastTab = $after
    $completedActions += 1
    if ($null -eq $after) { break }
  }

  $reused = $completedActions -eq $actions.Count -and $sequenceErrors.Count -eq 0
  Add-Observation "${Label}-share-stop-reuse" $Target $lastTab @{
    attachment = $Attachment
    reused = $reused
    fixed40x40 = $reused
    actionCount = $completedActions
    errors = @($sequenceErrors)
  }
  foreach ($errorText in $sequenceErrors) { [void]$failures.Add([string]$errorText) }
  return $lastTab
}

$observations = [System.Collections.Generic.List[object]]::new()
$failures = [System.Collections.Generic.List[string]]::new()
$followSamples = [System.Collections.Generic.List[object]]::new()
$startedSacrificial = $false
$qualityResult = $null
$activeActionsResult = $null
$positionResult = $null
$occlusionResult = $null
try {
  if ($LaunchSacrificial) {
    $targetHwnd = [PetalHoverTabSmoke]::StartSacrificialWindow()
    if ($targetHwnd -eq [IntPtr]::Zero) { throw 'Could not create the sacrificial WinForms window.' }
    [void][PetalHoverTabSmoke]::ActivateWindow($targetHwnd)
    $startedSacrificial = $true
    Start-Sleep -Milliseconds 100
  } else {
    if ($TargetHwnd -ne 0) {
      Write-Output 'No sacrificial process requested; using the explicitly supplied target HWND.'
      $targetHwnd = [IntPtr]$TargetHwnd
    } else {
      Write-Output 'No sacrificial process requested; the current cursor target is the positive control.'
      $targetHwnd = [PetalHoverTabSmoke]::TopmostAtCursor()
    }
    if ($targetHwnd -eq [IntPtr]::Zero) { throw 'No target window is under the cursor.' }
  }

  $target = Wait-ForFact { Get-Fact $targetHwnd } 'Positive-control target window'
  $center = Center-Fact $target
  if (-not [PetalHoverTabSmoke]::MoveCursor($center[0], $center[1])) { throw 'SetCursorPos failed for positive control.' }
  Start-Sleep -Milliseconds 100
  $tab = Wait-ForUnifiedTab $target 'Positive-control Hover Tab'
  $positiveErrors = @(Check-UnifiedGeometry $target $tab 'ordinary positive control')
  if ($LaunchSacrificial) {
    $positiveErrors += @(Check-TabAttachment $target $tab 'outside' 'ordinary positive control')
  }
  Add-Observation 'ordinary-positive-control' $target $tab @{ fixed40x40 = ($positiveErrors.Count -eq 0); attachment = if ($LaunchSacrificial) { 'outside' } else { 'unasserted' }; errors = $positiveErrors }
  foreach ($errorText in $positiveErrors) { $failures.Add([string]$errorText) }
  if ($ExerciseFollow) {
    $followControl = Invoke-FollowPositiveControl $target $targetHwnd
    foreach ($errorText in $followControl.Errors) { $failures.Add([string]$errorText) }
    if ($null -ne $followControl.Target) { $target = $followControl.Target }
    if ($null -ne $followControl.Tab) { $tab = $followControl.Tab }
  }
  if ($ExerciseOcclusion) {
    $occlusionResult = Invoke-OcclusionExercise $targetHwnd
    foreach ($errorText in $occlusionResult.Errors) { $failures.Add([string]$errorText) }
    if ($null -ne $occlusionResult.Target) { $target = $occlusionResult.Target }
    if ($null -ne $occlusionResult.Tab) { $tab = $occlusionResult.Tab }
  }

  if ($ExerciseShare) {
    $qualityResult = Invoke-NativeQualityPreset $target
    foreach ($errorText in $qualityResult.Errors) { $failures.Add([string]$errorText) }
  }
  if ($ExerciseShare -and $LaunchSacrificial) {
    $activeActionsResult = Invoke-ActiveShareMenuActions $target $targetHwnd
    foreach ($errorText in $activeActionsResult.Errors) { $failures.Add([string]$errorText) }
  }

  # Put the private source at the actual monitor work-area boundary before
  # exercising the taskbar shell negative control. This catches the old
  # full-monitor projection and verifies the tab survives cursor transfer and
  # direct actions while it remains inside Screen.WorkingArea.
  if ($Surface -eq 'taskbar' -and $LaunchSacrificial) {
    $taskbarEdge = Invoke-TaskbarEdgePlacement $targetHwnd $ExerciseShare
    foreach ($errorText in $taskbarEdge.Errors) { $failures.Add([string]$errorText) }
    if ($null -ne $taskbarEdge.Target) { $target = $taskbarEdge.Target }
    if ($null -ne $taskbarEdge.Tab) { $tab = $taskbarEdge.Tab }
  }

  # Exercise ordinary/outside continuity BEFORE maximization. This is the
  # regression path that used to lose the tab after Stop because the cursor
  # was left over desktop when the old token was retired.
  if ($ExerciseShare -and $LaunchSacrificial) {
    $reuseTab = Invoke-ShareStopReuse $target 'outside' 'ordinary-outside'
    if ($null -ne $reuseTab) { $tab = $reuseTab }
  }

  # Maximization proves the compact geometry and reuse behavior are invariant
  # after a long dwell, not merely immediately after the first hover.
  if ($LaunchSacrificial) {
    [void][PetalHoverTabSmoke]::Maximize($targetHwnd)
    Start-Sleep -Milliseconds 450
    $target = Get-Fact $targetHwnd
    $center = Center-Fact $target
    [void][PetalHoverTabSmoke]::MoveCursor($center[0], $center[1])
    Start-Sleep -Milliseconds 450
    $tab = Wait-ForUnifiedTab $target 'Maximized fixed Hover Tab'
    $maxErrors = @(Check-UnifiedGeometry $target $tab 'maximized fixed-geometry control')
    $maxErrors += @(Check-TabAttachment $target $tab 'inset' 'maximized fixed-geometry control')
    Add-Observation 'maximized-fixed-geometry-control' $target $tab @{ fixed40x40 = ($maxErrors.Count -eq 0); attachment = 'inset'; errors = $maxErrors; stationaryMilliseconds = 450 }
    foreach ($errorText in $maxErrors) { $failures.Add([string]$errorText) }
    if ($ExerciseShare) {
      $reuseTab = Invoke-ShareStopReuse $target 'inset' 'maximized-inset'
      if ($null -ne $reuseTab) { $tab = $reuseTab }
    }
  } elseif ($ExerciseShare) {
    $currentAttachment = Get-TabAttachment $target $tab
    if ($currentAttachment -eq 'unknown') {
      $failures.Add('current-window-share-stop-reuse: could not infer outside/inset attachment')
    } else {
      $reuseTab = Invoke-ShareStopReuse $target $currentAttachment 'current-window'
      if ($null -ne $reuseTab) { $tab = $reuseTab }
    }
  }

  # Right-click owns the options path. It must open a real native #32768 menu,
  # leave the fixed tab visible, and never advance the share lifecycle log.
  $tabCenter = Center-Fact $tab
  $beforeMenuMarkers = @(Get-ShareMarkers)
  if (-not [PetalHoverTabSmoke]::RightClickAt($tabCenter[0], $tabCenter[1])) {
    $failures.Add('right-click-native-menu: SendInput failed')
  }
  $nativeMenu = $null
  try { $nativeMenu = Wait-ForNativeMenu 'native share-options menu' } catch { $failures.Add("right-click-native-menu: $($_.Exception.Message)") }
  Start-Sleep -Milliseconds 150
  $menuTab = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
  $menuErrors = @(Check-UnifiedGeometry $target $menuTab 'native menu open')
  $afterMenuMarkers = @(Get-ShareMarkers)
  if ($afterMenuMarkers.Count -ne $beforeMenuMarkers.Count) {
    $menuErrors += 'right-click changed Share/Stop lifecycle markers'
  }
  Add-Observation 'right-click-native-menu' $target $menuTab @{ opened = ($null -ne $nativeMenu); toggledShare = ($afterMenuMarkers.Count -ne $beforeMenuMarkers.Count); fixed40x40 = ($menuErrors.Count -eq 0); errors = $menuErrors }
  foreach ($errorText in $menuErrors) { $failures.Add([string]$errorText) }

  [PetalHoverTabSmoke]::PressVirtualKey(0x1B)
  Start-Sleep -Milliseconds 250
  $menuAfterEscape = [PetalHoverTabSmoke]::FindVisibleClass('#32768')
  $escapeErrors = if ($menuAfterEscape -eq [IntPtr]::Zero) { @() } else { @('Escape did not close the native share-options menu') }
  $tabAfterEscape = Get-Fact ([PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab'))
  $escapeGeometryErrors = @(Check-UnifiedGeometry $target $tabAfterEscape 'menu closed')
  $escapeErrors += $escapeGeometryErrors
  Add-Observation 'native-menu-escape' $target $tabAfterEscape @{ closed = ($menuAfterEscape -eq [IntPtr]::Zero); fixed40x40 = ($escapeErrors.Count -eq 0); errors = $escapeErrors }
  foreach ($errorText in $escapeErrors) { $failures.Add([string]$errorText) }

  if ($Surface -ne 'none') {
    $surfaceNames = if ($Surface -eq 'all') { @('start', 'notifications', 'quick-settings', 'taskbar') } else { @($Surface) }
    foreach ($surfaceName in $surfaceNames) {
      # Close the previous flyout before opening the next one. This also makes
      # the foreground-window lookup below a fresh observation, not stale state.
      [PetalHoverTabSmoke]::PressVirtualKey(0x1B)
      Start-Sleep -Milliseconds 150
      if ($LaunchSacrificial) {
        # Return focus to the known ordinary target before sending a shell
        # chord. Otherwise a prior flyout or unrelated foreground app may
        # consume the synthetic Windows-key event.
        [void][PetalHoverTabSmoke]::ActivateWindow($targetHwnd)
        $target = Get-Fact $targetHwnd
        $center = Center-Fact $target
        [void][PetalHoverTabSmoke]::MoveCursor($center[0], $center[1])
        Start-Sleep -Milliseconds 100
      }
      switch ($surfaceName) {
        'start' { [PetalHoverTabSmoke]::PressVirtualKey(0x5B) | Out-Null }
        'notifications' { [PetalHoverTabSmoke]::PressChord(0x5B, 0x4E) | Out-Null }
        'quick-settings' { [PetalHoverTabSmoke]::PressChord(0x5B, 0x41) | Out-Null }
        'taskbar' {
          $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
          [PetalHoverTabSmoke]::MoveCursor([int]($bounds.Left + $bounds.Width / 2), [int]($bounds.Bottom - 4)) | Out-Null
        }
        'current' { }
      }
      Start-Sleep -Milliseconds 500
      if ($surfaceName -eq 'current') {
        $surfaceTarget = Wait-ForFact { [PetalHoverTabSmoke]::FactAtCursor($petalPid) } "Current surface '$surfaceName'"
      } else {
        $surfaceHwnd = Wait-ForShellSurface $surfaceName
        $surfaceFrame = Wait-ForFact { Get-Fact $surfaceHwnd } "Shell surface '$surfaceName' frame"
        $surfaceCenter = Center-Fact $surfaceFrame
        if ($surfaceName -eq 'quick-settings') {
          # ControlCenterWindow reports a full-height transparent frame, but
          # only its lower tray-side region is hit-testable.
          $surfaceCenter[1] = [int]($surfaceFrame.y + $surfaceFrame.height -
            [Math]::Min((Expected-Pixels 160 $surfaceFrame), $surfaceFrame.height / 4))
        }
        if (-not [PetalHoverTabSmoke]::MoveCursor($surfaceCenter[0], $surfaceCenter[1])) {
          throw "SetCursorPos failed for shell surface '$surfaceName'."
        }
        Start-Sleep -Milliseconds 300
        $surfaceTarget = Wait-ForFact { [PetalHoverTabSmoke]::FactAtCursor($petalPid) } "Shell surface '$surfaceName' under cursor"
        # Allow the 16ms hover tracker to process the rejected shell surface
        # before sampling whether the native pill has disappeared.
        Start-Sleep -Milliseconds 300
      }
      $surfaceTabHwnd = [PetalHoverTabSmoke]::FindVisibleTitle('Hover Tab')
      $surfaceTab = Get-Fact $surfaceTabHwnd
      $surfaceErrors = [System.Collections.Generic.List[string]]::new()
      if ($null -ne $surfaceTab) { $surfaceErrors.Add("${surfaceName}: Hover Tab remained visible over the observed surface") }
      if ($surfaceName -ne 'current' -and $surfaceTarget.pickerDecision -ne 'rejected-system-surface') {
        $surfaceErrors.Add("${surfaceName}: diagnostic shell decision was '$($surfaceTarget.pickerDecision)' for $($surfaceTarget.className)/$($surfaceTarget.processName)")
      }
      Add-Observation "surface-$surfaceName" $surfaceTarget $surfaceTab @{ blocked = ($surfaceErrors.Count -eq 0); errors = $surfaceErrors }
      foreach ($errorText in $surfaceErrors) { $failures.Add([string]$errorText) }
      [PetalHoverTabSmoke]::PressVirtualKey(0x1B)
      Start-Sleep -Milliseconds 150
    }
  }

  if ($ExercisePosition) {
    if ($LaunchSacrificial) {
      [void][PetalHoverTabSmoke]::ActivateWindow($targetHwnd)
    }
    $target = Get-Fact $targetHwnd
    if ($null -ne $target) {
      $center = Center-Fact $target
      [void][PetalHoverTabSmoke]::MoveCursor($center[0], $center[1])
      Start-Sleep -Milliseconds 180
      $positionResult = Invoke-NativePositionPreset $target $targetHwnd 'bottom'
      foreach ($errorText in $positionResult.Errors) { $failures.Add([string]$errorText) }
      if ($null -ne $positionResult.Target) { $target = $positionResult.Target }
      if ($null -ne $positionResult.Tab) { $tab = $positionResult.Tab }
    } else {
      $failures.Add('native-position-preset: target disappeared before position exercise')
    }
  }

  $followVisibleSamples = @($followSamples | Where-Object { $_.targetVisible })
  $followVisibilityGaps = @($followVisibleSamples | Where-Object { $_.visibilityGap }).Count
  $followBorderCurrentTabPrevious = @($followSamples | Where-Object { $_.borderCurrentTabPrevious }).Count
  $followTabErrors = @($followSamples | Where-Object { $null -ne $_.tabEdgeErrorPx } | ForEach-Object { [double]$_.tabEdgeErrorPx })
  $followBorderErrors = @($followSamples | Where-Object { $null -ne $_.borderEdgeErrorPx } | ForEach-Object { [double]$_.borderEdgeErrorPx })
  $evidence = [pscustomobject]@{
    schema = 'petal.windows-hover-tab-smoke.v6'
    observedAtUtc = [DateTime]::UtcNow.ToString('o')
    mode = 'unified-right-edge-rail'
    petalDevProcessGate = [pscustomobject]@{
      status = 'passed'
      ownedPid = $petalPid
      executablePath = $ownedPath
      foreignPetalCount = $foreignPetal.Count
    }
    sacrificialPid = if ($startedSacrificial) { $PID } else { $null }
    preferences = [pscustomobject]@{
      path = Get-SharePreferencePath
      priority = if ($null -eq $qualityResult -or $null -eq $qualityResult.Preferences) { $null } else { $qualityResult.Preferences.priority }
      hoverTabVerticalOffset = if ($null -eq $positionResult -or $null -eq $positionResult.Preferences) { $null } else { $positionResult.Preferences.hoverTabVerticalOffset }
      qualityRequested = ($null -ne $qualityResult)
      positionRequested = ($null -ne $positionResult)
      activeShareActionsRequested = ($null -ne $activeActionsResult)
      activeShareActionsVerified = ($null -ne $activeActionsResult -and $activeActionsResult.Errors.Count -eq 0)
    }
    follow = [pscustomobject]@{
      requested = $ExerciseFollow
      sampleCount = $followSamples.Count
      visibilityGapSamples = $followVisibilityGaps
      borderCurrentTabPreviousSamples = $followBorderCurrentTabPrevious
      maxTabEdgeErrorPx = if ($followTabErrors.Count -eq 0) { $null } else { [math]::Round(([double]($followTabErrors | Measure-Object -Maximum).Maximum), 1) }
      maxBorderEdgeErrorPx = if ($followBorderErrors.Count -eq 0) { $null } else { [math]::Round(([double]($followBorderErrors | Measure-Object -Maximum).Maximum), 1) }
      samples = @($followSamples)
    }
    occlusion = [pscustomobject]@{
      requested = $ExerciseOcclusion
      verified = ($null -ne $occlusionResult -and $occlusionResult.Errors.Count -eq 0)
      errors = if ($null -eq $occlusionResult) { @() } else { @($occlusionResult.Errors) }
    }
    observations = @($observations)
    failures = @($failures)
  }
  $evidence | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $EvidencePath -Encoding UTF8
  Write-Output "Evidence=$EvidencePath"
  $observations | ForEach-Object {
    $tabText = if ($_.hoverTab) { "$($_.hoverTab.width)x$($_.hoverTab.height) at $($_.hoverTab.x),$($_.hoverTab.y)" } else { 'hidden' }
    Write-Output "Observation=$($_.name) target=$($_.target.className)/$($_.target.processName) tab=$tabText"
  }
  if ($failures.Count -gt 0) {
    Write-Error ("RED/FAIL: unified hover-tab contract has $($failures.Count) failure(s): " + ($failures -join '; '))
    exit 2
  }
  Write-Output 'PASS: unified right-center hover-tab smoke.'
} finally {
  if ($startedSacrificial -and -not $KeepSacrificial) {
    [PetalHoverTabSmoke]::StopSacrificialWindow()
    if (-not [PetalHoverTabSmoke]::WaitForSacrificialStopped(5000)) {
      throw 'FATAL: the recorded sacrificial window thread did not terminate during teardown.'
    }
    Write-Output 'Teardown=recorded sacrificial window stopped'
  }
}
