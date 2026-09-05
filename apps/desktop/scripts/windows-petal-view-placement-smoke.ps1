#Requires -Version 5.1
<##
.SYNOPSIS
  Bounded Windows smoke exercise for Petal View placement input.

.DESCRIPTION
  Start Petal View in follow-cursor placement mode, then run this script while
  the selector is visible over the meeting window. The script finds the
  visible Petal View top-level window, moves the real cursor to its center,
  sends a real SendInput left click, captures the virtual desktop, and prints
  a bounded tail of Petal's native log. The operator checks the screenshot and
  log to confirm that the meeting window did not receive the placement click.

  This is deliberately an operator exercise rather than a fake DOM test:
  placement crosses the native hit-test boundary and must be checked with the
  real Windows input queue. It never activates or foregrounds a window.

.EXAMPLE
  .\windows-petal-view-placement-smoke.ps1

  Run immediately after choosing the follow-cursor Petal View action.

.EXAMPLE
  .\windows-petal-view-placement-smoke.ps1 -WindowTitlePrefix 'Petal View' -TimeoutSeconds 45
#>

[CmdletBinding()]
param(
  [string]$WindowTitlePrefix = 'Petal View',
  [ValidateRange(5, 300)]
  [int]$TimeoutSeconds = 30,
  [string]$ScreenshotPath = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $ScreenshotPath) {
  $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
  $ScreenshotPath = Join-Path ([IO.Path]::GetTempPath()) "petal-view-placement-$stamp.png"
}
$logPath = if ($env:APPDATA) {
  Join-Path $env:APPDATA 'Petal\logs\petal.log'
} else {
  Join-Path ([IO.Path]::GetTempPath()) 'Petal\logs\petal.log'
}

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies ([System.Drawing.Image].Assembly.Location) -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;

public static class PetalViewPlacementSmoke {
    private const uint INPUT_MOUSE = 0;
    private const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    private const uint MOUSEEVENTF_LEFTUP = 0x0004;
    private const int SM_XVIRTUALSCREEN = 76;
    private const int SM_YVIRTUALSCREEN = 77;
    private const int SM_CXVIRTUALSCREEN = 78;
    private const int SM_CYVIRTUALSCREEN = 79;

    [StructLayout(LayoutKind.Sequential)]
    private struct Rect {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MouseInput {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }

    // INPUT contains a 32-byte union on x64. The explicit layout keeps the
    // native SendInput ABI correct instead of relying on PowerShell's object
    // marshalling for a C union.
    [StructLayout(LayoutKind.Explicit, Size = 40)]
    private struct Input {
        [FieldOffset(0)] public uint Type;
        [FieldOffset(8)] public MouseInput Mouse;
    }

    private delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int maxCount);

    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    private static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);

    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out Point point);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint SendInput(uint count, Input[] inputs, int inputSize);

    [DllImport("user32.dll")]
    private static extern int GetSystemMetrics(int index);

    public static IntPtr FindVisibleWindow(string titlePrefix) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            if (!IsWindowVisible(hwnd)) return true;
            var text = new StringBuilder(512);
            GetWindowText(hwnd, text, text.Capacity);
            if (text.ToString().StartsWith(titlePrefix, StringComparison.OrdinalIgnoreCase)) {
                found = hwnd;
                return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static int[] Center(IntPtr hwnd) {
        Rect rect;
        if (!GetWindowRect(hwnd, out rect)) return null;
        return new[] { (rect.Left + rect.Right) / 2, (rect.Top + rect.Bottom) / 2,
                       rect.Left, rect.Top, rect.Right, rect.Bottom };
    }

    public static bool ClickAt(int x, int y) {
        if (!SetCursorPos(x, y)) return false;
        var inputs = new[] {
            new Input { Type = INPUT_MOUSE, Mouse = new MouseInput { Flags = MOUSEEVENTF_LEFTDOWN } },
            new Input { Type = INPUT_MOUSE, Mouse = new MouseInput { Flags = MOUSEEVENTF_LEFTUP } }
        };
        return SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(Input))) == inputs.Length;
    }

    public static void SaveVirtualScreen(string path) {
        var left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        var top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        var width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        var height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if (width <= 0 || height <= 0) throw new InvalidOperationException("virtual desktop has no pixels");
        using (var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb))
        using (var graphics = Graphics.FromImage(bitmap)) {
            graphics.CopyFromScreen(left, top, 0, 0, new Size(width, height), CopyPixelOperation.SourceCopy);
            bitmap.Save(path, ImageFormat.Png);
        }
    }
}
'@

$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
$hwnd = [IntPtr]::Zero
while ($hwnd -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline) {
  $hwnd = [PetalViewPlacementSmoke]::FindVisibleWindow($WindowTitlePrefix)
  if ($hwnd -eq [IntPtr]::Zero) {
    Start-Sleep -Milliseconds 250
  }
}
if ($hwnd -eq [IntPtr]::Zero) {
  throw "No visible Petal View window with title prefix '$WindowTitlePrefix' appeared within $TimeoutSeconds seconds. Start follow-cursor placement first."
}

$beforeMarkers = @()
if (Test-Path -LiteralPath $logPath) {
  $beforeMarkers = @(Get-Content -LiteralPath $logPath -Tail 200 | Select-String -SimpleMatch 'cursor placement started')
}
$center = [PetalViewPlacementSmoke]::Center($hwnd)
if ($null -eq $center) {
  throw 'Petal View disappeared before its bounds could be read.'
}
Write-Output "Petal View HWND=$hwnd bounds=($($center[2]),$($center[3]))-$($center[4]),$($center[5]) click=($($center[0]),$($center[1]))"

if (-not [PetalViewPlacementSmoke]::ClickAt($center[0], $center[1])) {
  throw 'SendInput did not submit the placement click.'
}
Start-Sleep -Milliseconds 350
[PetalViewPlacementSmoke]::SaveVirtualScreen($ScreenshotPath)

$afterMarkers = @()
$tail = @()
if (Test-Path -LiteralPath $logPath) {
  $tail = @(Get-Content -LiteralPath $logPath -Tail 120)
  $afterMarkers = @($tail | Select-String -SimpleMatch 'cursor placement started')
}

Write-Output "Screenshot=$ScreenshotPath"
Write-Output "Log=$logPath"
Write-Output "Placement-start markers before=$($beforeMarkers.Count) after=$($afterMarkers.Count)"
Write-Output '--- bounded Petal View placement log tail (last 120 lines) ---'
$tail | Where-Object {
  $_ -match 'region window|share|capture|border|placement'
}
Write-Output '--- end bounded log tail ---'
Write-Output 'Manual verdict: inspect the PNG and confirm the meeting window stayed visible and no underlying control reacted to this click.'
