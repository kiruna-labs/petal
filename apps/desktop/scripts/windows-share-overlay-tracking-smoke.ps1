#Requires -Version 5.1
<##
.SYNOPSIS
  Measure the native Petal sharer overlay tracker against a live share.

.DESCRIPTION
  This is an opt-in interactive red/green exercise. Start Petal, join a
  meeting, share one resizable window, then pass an unambiguous substring of
  that source window's title. The script finds the source and its existing
  `Petal Sharer Pointer` WebView, drives native move/resize,
  maximize/restore, minimize/restore, and title-bar activation transitions,
  and samples both rectangles at 8ms by default.

  Source geometry is measured with DWMWA_EXTENDED_FRAME_BOUNDS because that is
  the visible rectangle. The overlay is measured with GetWindowRect. A sample
  is a hybrid when the overlay matches neither the previous nor current source
  visible frame. In `Owned` mode, a visible source with a hidden overlay is a
  visibility gap, a wrong GW_OWNER is an ownership gap, and an overlay below
  its source is a stacking gap. `Passive` mode intentionally does not require
  ownership or stacking: it verifies the WGC system-indicator log instead.
  The overlay's GW_OWNER is recorded directly. An optional occluder title
  verifies that an unrelated overlapping foreground window remains above both
  source and overlay.

  The exercise must remain red for real visibility, geometry, ownership, or
  reload failures. It must not turn those observations into a pass by widening
  tolerances.

.EXAMPLE
  .\windows-share-overlay-tracking-smoke.ps1 -SourceWindowTitle 'Notepad' -Mode Owned

.EXAMPLE
  .\windows-share-overlay-tracking-smoke.ps1 -SourceWindowTitle 'Notepad' `
    -Mode Owned -SampleMilliseconds 8 -ActionTimeoutMilliseconds 1200

.EXAMPLE
  .\windows-share-overlay-tracking-smoke.ps1 -SourceWindowTitle 'Task Manager' `
    -Mode Passive -OccluderWindowTitle 'Notepad'
#>

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string]$SourceWindowTitle,
  [ValidateSet('Owned', 'Passive')] [string]$Mode = 'Owned',
  [ValidateRange(1, 100)] [int]$SampleMilliseconds = 8,
  [ValidateRange(250, 5000)] [int]$ActionTimeoutMilliseconds = 1200,
  [ValidateRange(1, 60)] [int]$TimeoutSeconds = 10,
  [ValidateRange(1, 32)] [int]$SettledEdgeTolerancePixels = 2,
  [string]$OccluderWindowTitle = '',
  [string]$CsvPath = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;

public static class PetalShareOverlayTrackingNative {
    public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT {
        public int dx;
        public int dy;
        public uint mouseData;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT {
        public uint type;
        public MOUSEINPUT mi;
    }

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int maxCount);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool IsZoomed(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern IntPtr GetWindow(IntPtr hwnd, uint command);

    [DllImport("user32.dll")]
    public static extern bool SetWindowPos(
        IntPtr hwnd,
        IntPtr insertAfter,
        int x,
        int y,
        int width,
        int height,
        uint flags);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hwnd, int command);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern uint SendInput(uint inputCount, INPUT[] inputs, int inputSize);

    [DllImport("dwmapi.dll")]
    public static extern int DwmGetWindowAttribute(
        IntPtr hwnd,
        int attribute,
        out RECT value,
        int valueSize);
}
'@

$DWMWA_EXTENDED_FRAME_BOUNDS = 9
$GW_HWNDPREV = 3
$GW_OWNER = 4
$MAX_Z_ORDER_WALK = 64
$SWP_NOZORDER = 0x0004
$SWP_NOACTIVATE = 0x0010
$SW_MINIMIZE = 6
$SW_RESTORE = 9
$SW_MAXIMIZE = 3
$MOUSEEVENTF_LEFTDOWN = 0x0002
$MOUSEEVENTF_LEFTUP = 0x0004
$INPUT_MOUSE = 0

function Convert-NativeRect {
  param([Parameter(Mandatory = $true)] $Rect)
  $width = $Rect.Right - $Rect.Left
  $height = $Rect.Bottom - $Rect.Top
  if ($width -le 0 -or $height -le 0) { return $null }
  [pscustomobject]@{
    X = [int]$Rect.Left
    Y = [int]$Rect.Top
    Width = [int]$width
    Height = [int]$height
  }
}

function Get-WindowText {
  param([Parameter(Mandatory = $true)] [IntPtr]$Hwnd)
  $text = New-Object System.Text.StringBuilder 512
  [void][PetalShareOverlayTrackingNative]::GetWindowText($Hwnd, $text, $text.Capacity)
  $text.ToString()
}

function Get-WindowRecord {
  param([Parameter(Mandatory = $true)] [IntPtr]$Hwnd)
  $rect = New-Object PetalShareOverlayTrackingNative+RECT
  if (-not [PetalShareOverlayTrackingNative]::GetWindowRect($Hwnd, [ref]$rect)) { return $null }
  $frame = Convert-NativeRect $rect
  if ($null -eq $frame) { return $null }
  [uint32]$processId = 0
  [void][PetalShareOverlayTrackingNative]::GetWindowThreadProcessId($Hwnd, [ref]$processId)
  [pscustomobject]@{
    Hwnd = $Hwnd
    ProcessId = $processId
    Title = Get-WindowText $Hwnd
    Visible = [PetalShareOverlayTrackingNative]::IsWindowVisible($Hwnd)
    Minimized = [PetalShareOverlayTrackingNative]::IsIconic($Hwnd)
    Maximized = [PetalShareOverlayTrackingNative]::IsZoomed($Hwnd)
    Frame = $frame
  }
}

function Get-AllTopLevelWindows {
  $windows = New-Object 'System.Collections.Generic.List[object]'
  $callback = [PetalShareOverlayTrackingNative+EnumWindowsProc] {
    param($Hwnd, $LParam)
    $record = Get-WindowRecord $Hwnd
    if ($null -ne $record) { $windows.Add($record) }
    return $true
  }
  [void][PetalShareOverlayTrackingNative]::EnumWindows($callback, [IntPtr]::Zero)
  $windows.ToArray()
}

function Get-SourceWindow {
  param([Parameter(Mandatory = $true)] [string]$TitleSubstring)
  Get-AllTopLevelWindows |
    Where-Object {
      $_.Visible -and -not $_.Minimized -and $_.Title.IndexOf($TitleSubstring, [StringComparison]::OrdinalIgnoreCase) -ge 0
    } |
    Sort-Object @{ Expression = { $_.Frame.Width * $_.Frame.Height }; Descending = $true } |
    Select-Object -First 1
}

function Get-VisibleWindowFrame {
  param([Parameter(Mandatory = $true)] [IntPtr]$Hwnd)
  $dwmRect = New-Object PetalShareOverlayTrackingNative+RECT
  $size = [Runtime.InteropServices.Marshal]::SizeOf([type]'PetalShareOverlayTrackingNative+RECT')
  $hr = [PetalShareOverlayTrackingNative]::DwmGetWindowAttribute(
    $Hwnd,
    $DWMWA_EXTENDED_FRAME_BOUNDS,
    [ref]$dwmRect,
    $size
  )
  if ($hr -eq 0) {
    $dwmFrame = Convert-NativeRect $dwmRect
    if ($null -ne $dwmFrame) { return $dwmFrame }
  }
  $rect = New-Object PetalShareOverlayTrackingNative+RECT
  if (-not [PetalShareOverlayTrackingNative]::GetWindowRect($Hwnd, [ref]$rect)) { return $null }
  Convert-NativeRect $rect
}

function Get-OverlayWindow {
  param(
    [Parameter(Mandatory = $true)] $Source,
    [Parameter(Mandatory = $true)] [string]$OverlayTitle
  )
  $sourceFrame = Get-VisibleWindowFrame $Source.Hwnd
  Get-AllTopLevelWindows |
    Where-Object {
      $_.Visible -and $_.Title -eq $OverlayTitle -and $_.Hwnd -ne $Source.Hwnd
    } |
    ForEach-Object {
      $distance = if ($null -eq $sourceFrame) { [double]::MaxValue } else {
        [Math]::Abs($_.Frame.X - $sourceFrame.X) +
        [Math]::Abs($_.Frame.Y - $sourceFrame.Y) +
        [Math]::Abs($_.Frame.Width - $sourceFrame.Width) +
        [Math]::Abs($_.Frame.Height - $sourceFrame.Height)
      }
      $_ | Add-Member -NotePropertyName DistanceToSource -NotePropertyValue $distance -PassThru
    } |
    Sort-Object DistanceToSource |
    Select-Object -First 1
}

function Test-FrameWithin {
  param($Actual, $Expected, [int]$Tolerance)
  if ($null -eq $Actual -or $null -eq $Expected) { return $false }
  return (
    [Math]::Abs($Actual.X - $Expected.X) -le $Tolerance -and
    [Math]::Abs($Actual.Y - $Expected.Y) -le $Tolerance -and
    [Math]::Abs($Actual.Width - $Expected.Width) -le $Tolerance -and
    [Math]::Abs($Actual.Height - $Expected.Height) -le $Tolerance
  )
}

function Get-FrameError {
  param($Actual, $Expected)
  if ($null -eq $Actual -or $null -eq $Expected) { return [double]::PositiveInfinity }
  [double]([Math]::Max(
    [Math]::Max([Math]::Abs($Actual.X - $Expected.X), [Math]::Abs($Actual.Y - $Expected.Y)),
    [Math]::Max([Math]::Abs($Actual.Width - $Expected.Width), [Math]::Abs($Actual.Height - $Expected.Height))
  ))
}

function Get-WindowOwner {
  param([Parameter(Mandatory = $true)] [IntPtr]$Hwnd)
  [PetalShareOverlayTrackingNative]::GetWindow($Hwnd, $GW_OWNER)
}

function Test-OverlayOwner {
  param(
    [Parameter(Mandatory = $true)] [IntPtr]$OverlayHwnd,
    [Parameter(Mandatory = $true)] [IntPtr]$SourceHwnd
  )
  (Get-WindowOwner $OverlayHwnd) -eq $SourceHwnd
}

function Test-WindowAbove {
  param(
    [Parameter(Mandatory = $true)] [IntPtr]$AboveHwnd,
    [Parameter(Mandatory = $true)] [IntPtr]$BelowHwnd
  )
  $candidate = [PetalShareOverlayTrackingNative]::GetWindow($BelowHwnd, $GW_HWNDPREV)
  for ($i = 0; $i -lt $MAX_Z_ORDER_WALK; $i++) {
    if ($candidate -eq [IntPtr]::Zero) { return $false }
    if ($candidate -eq $AboveHwnd) { return $true }
    $candidate = [PetalShareOverlayTrackingNative]::GetWindow($candidate, $GW_HWNDPREV)
  }
  return $false
}

function Test-RectsOverlap {
  param($First, $Second)
  if ($null -eq $First -or $null -eq $Second) { return $false }
  return $First.X -lt ($Second.X + $Second.Width) -and
    ($First.X + $First.Width) -gt $Second.X -and
    $First.Y -lt ($Second.Y + $Second.Height) -and
    ($First.Y + $First.Height) -gt $Second.Y
}

function Send-TitleBarClick {
  param([Parameter(Mandatory = $true)] $Source)
  $outer = (Get-WindowRecord $Source.Hwnd).Frame
  $x = $outer.X + [Math]::Max(16, [int]($outer.Width / 2))
  $y = $outer.Y + 18
  [void][PetalShareOverlayTrackingNative]::SetCursorPos($x, $y)
  $inputs = New-Object 'PetalShareOverlayTrackingNative+INPUT[]' 2
  $inputs[0].type = $INPUT_MOUSE
  $inputs[0].mi.dwFlags = $MOUSEEVENTF_LEFTDOWN
  $inputs[1].type = $INPUT_MOUSE
  $inputs[1].mi.dwFlags = $MOUSEEVENTF_LEFTUP
  $sent = [PetalShareOverlayTrackingNative]::SendInput(2, $inputs, [Runtime.InteropServices.Marshal]::SizeOf($inputs[0]))
  if ($sent -ne 2) { throw "SendInput title-bar click sent $sent/2 inputs" }
}

function Set-SourcePosition {
  param(
    [Parameter(Mandatory = $true)] $Source,
    [Parameter(Mandatory = $true)] [int]$X,
    [Parameter(Mandatory = $true)] [int]$Y,
    [Parameter(Mandatory = $true)] [int]$Width,
    [Parameter(Mandatory = $true)] [int]$Height
  )
  $flags = $SWP_NOZORDER -bor $SWP_NOACTIVATE
  if (-not [PetalShareOverlayTrackingNative]::SetWindowPos($Source.Hwnd, [IntPtr]::Zero, $X, $Y, $Width, $Height, $flags)) {
    throw "SetWindowPos failed for source HWND $($Source.Hwnd)"
  }
}

function Get-LogFirstPaintCount {
  param([string]$LogPath)
  if (-not (Test-Path -LiteralPath $LogPath)) { return 0 }
  @(Get-Content -LiteralPath $LogPath | Where-Object { $_ -match "frontend first paint -- window='petal-sharer-pointer-" }).Count
}

function Get-LogTail {
  param([string]$LogPath)
  if (-not (Test-Path -LiteralPath $LogPath)) { return @() }
  @(Get-Content -LiteralPath $LogPath -Tail 400)
}

$logPath = if ($env:APPDATA) {
  Join-Path $env:APPDATA 'Petal\logs\petal.log'
} else {
  Join-Path ([IO.Path]::GetTempPath()) 'Petal\logs\petal.log'
}
$overlayTitle = 'Petal Sharer Pointer'
$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
$source = $null
$overlay = $null
$occluder = $null
while ([DateTime]::UtcNow -lt $deadline) {
  $source = Get-SourceWindow $SourceWindowTitle
  if ($null -ne $source) {
    $overlay = Get-OverlayWindow $source $overlayTitle
    if ($null -ne $overlay) { break }
  }
  Start-Sleep -Milliseconds 250
}
if ($null -eq $source) {
  throw "No visible, non-minimized source window matched '$SourceWindowTitle' within $TimeoutSeconds seconds. Start a $Mode share first."
}
if ($null -eq $overlay) {
  throw "No visible '$overlayTitle' overlay was paired with source '$($source.Title)' within $TimeoutSeconds seconds. This exercise requires an active $Mode share."
}
if ($OccluderWindowTitle) {
  $occluder = Get-SourceWindow $OccluderWindowTitle
  if ($null -eq $occluder) {
    throw "No visible, non-minimized occluder matched '$OccluderWindowTitle'."
  }
  if ($occluder.Hwnd -eq $source.Hwnd -or $occluder.Hwnd -eq $overlay.Hwnd) {
    throw 'Occluder must be a different top-level window from the source and Petal overlay.'
  }
}

$sourceOuterBefore = (Get-WindowRecord $source.Hwnd).Frame
$sourceVisibleBefore = Get-VisibleWindowFrame $source.Hwnd
$sourceWasMinimized = $source.Minimized
$sourceWasMaximized = $source.Maximized
$cursorBefore = [System.Windows.Forms.Cursor]::Position
$firstPaintBefore = Get-LogFirstPaintCount $logPath
$samples = New-Object 'System.Collections.Generic.List[object]'
$actions = New-Object 'System.Collections.Generic.List[object]'
$lastExpected = $sourceVisibleBefore
$scriptStart = [System.Diagnostics.Stopwatch]::StartNew()

function Add-TrackingSample {
  param(
    [Parameter(Mandatory = $true)] [string]$Action,
    [Parameter(Mandatory = $true)] [double]$ActionElapsedMs,
    [Parameter(Mandatory = $true)] $PreviousExpected
  )
  $sourceRecord = Get-WindowRecord $source.Hwnd
  $expected = if ($null -eq $sourceRecord) { $null } else { Get-VisibleWindowFrame $source.Hwnd }
  $actualRecord = Get-WindowRecord $overlay.Hwnd
  $actual = if ($null -eq $actualRecord) { $null } else { $actualRecord.Frame }
  $visible = $null -ne $actualRecord -and $actualRecord.Visible -and -not $actualRecord.Minimized
  $sourceVisible = $null -ne $sourceRecord -and $sourceRecord.Visible -and -not $sourceRecord.Minimized
  $error = Get-FrameError $actual $expected
  $matchesCurrent = Test-FrameWithin $actual $expected $SettledEdgeTolerancePixels
  $matchesPrevious = Test-FrameWithin $actual $PreviousExpected $SettledEdgeTolerancePixels
  $hybrid = $sourceVisible -and $visible -and -not $matchesCurrent -and -not $matchesPrevious
  $owner = if ($null -eq $actualRecord) { $null } else { Get-WindowOwner $overlay.Hwnd }
  $ownerOkay = if ($Mode -eq 'Owned' -and $sourceVisible -and $visible) {
    Test-OverlayOwner $overlay.Hwnd $source.Hwnd
  } else { $true }
  $stackingOkay = if ($Mode -eq 'Owned' -and $sourceVisible -and $visible) {
    Test-WindowAbove $overlay.Hwnd $source.Hwnd
  } else { $true }
  $sample = [pscustomobject]@{
    AtMs = [math]::Round($scriptStart.Elapsed.TotalMilliseconds, 1)
    Action = $Action
    ActionElapsedMs = [math]::Round($ActionElapsedMs, 1)
    SourceVisible = $sourceVisible
    OverlayVisible = $visible
    OwnerOkay = $ownerOkay
    OwnerHwnd = if ($null -eq $owner) { $null } else { '0x{0:X}' -f $owner.ToInt64() }
    StackingOkay = $stackingOkay
    Hybrid = $hybrid
    EdgeErrorPx = if ([double]::IsInfinity($error)) { $null } else { [math]::Round($error, 1) }
    SourceX = if ($null -eq $expected) { $null } else { $expected.X }
    SourceY = if ($null -eq $expected) { $null } else { $expected.Y }
    SourceWidth = if ($null -eq $expected) { $null } else { $expected.Width }
    SourceHeight = if ($null -eq $expected) { $null } else { $expected.Height }
    OverlayX = if ($null -eq $actual) { $null } else { $actual.X }
    OverlayY = if ($null -eq $actual) { $null } else { $actual.Y }
    OverlayWidth = if ($null -eq $actual) { $null } else { $actual.Width }
    OverlayHeight = if ($null -eq $actual) { $null } else { $actual.Height }
  }
  $samples.Add($sample)
  $sample
}

function Wait-TrackingState {
  param(
    [Parameter(Mandatory = $true)] [string]$Action,
    [Parameter(Mandatory = $true)] [bool]$ExpectVisible,
    [Parameter(Mandatory = $true)] [double]$StartedAt,
    [Parameter(Mandatory = $true)] $PreviousExpected
  )
  $deadline = [System.Diagnostics.Stopwatch]::GetTimestamp() +
    [int64]($ActionTimeoutMilliseconds * ([System.Diagnostics.Stopwatch]::Frequency / 1000.0))
  $firstSettledMs = $null
  do {
    $elapsed = ([System.Diagnostics.Stopwatch]::GetTimestamp() - $StartedAt) * 1000.0 / [System.Diagnostics.Stopwatch]::Frequency
    $sample = Add-TrackingSample $Action $elapsed $PreviousExpected
    $settled = if ($ExpectVisible) {
      $sample.SourceVisible -and $sample.OverlayVisible -and $sample.OwnerOkay -and
        $sample.StackingOkay -and $sample.EdgeErrorPx -le $SettledEdgeTolerancePixels
    } else {
      -not $sample.OverlayVisible
    }
    if ($settled -and $null -eq $firstSettledMs) { $firstSettledMs = $elapsed }
    if ($settled) { break }
    Start-Sleep -Milliseconds $SampleMilliseconds
  } while ([System.Diagnostics.Stopwatch]::GetTimestamp() -lt $deadline)
  [pscustomobject]@{
    Name = $Action
    Settled = $null -ne $firstSettledMs
    LatencyMs = if ($null -eq $firstSettledMs) { $null } else { [math]::Round($firstSettledMs, 1) }
  }
}

function Invoke-SourceAction {
  param(
    [Parameter(Mandatory = $true)] [string]$Name,
    [Parameter(Mandatory = $true)] [scriptblock]$Action,
    [Parameter(Mandatory = $true)] [bool]$ExpectVisible
  )
  $previousExpected = Get-VisibleWindowFrame $source.Hwnd
  $startedAt = [System.Diagnostics.Stopwatch]::GetTimestamp()
  & $Action
  $result = Wait-TrackingState $Name $ExpectVisible $startedAt $previousExpected
  $actions.Add($result)
}

function Invoke-ContinuousMove {
  param([Parameter(Mandatory = $true)] $Source)
  $name = 'continuous-move'
  $previousExpected = Get-VisibleWindowFrame $Source.Hwnd
  $startedAt = [System.Diagnostics.Stopwatch]::GetTimestamp()
  $outer = (Get-WindowRecord $Source.Hwnd).Frame
  for ($step = 1; $step -le 60; $step++) {
    Set-SourcePosition $Source ($outer.X + $step * 3) ($outer.Y + $step * 2) $outer.Width $outer.Height
    $elapsed = ([System.Diagnostics.Stopwatch]::GetTimestamp() - $startedAt) * 1000.0 / [System.Diagnostics.Stopwatch]::Frequency
    [void](Add-TrackingSample $name $elapsed $previousExpected)
    Start-Sleep -Milliseconds $SampleMilliseconds
  }
  $result = Wait-TrackingState $name $true $startedAt $previousExpected
  $actions.Add([pscustomobject]@{
    Name = $name
    Settled = $result.Settled
    LatencyMs = $result.LatencyMs
  })
}

try {
  # Ensure the pair starts foregrounded and the overlay is observed before any
  # mutation. This also makes the expected source/overlay ownership meaningful.
  [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_RESTORE)
  [void][PetalShareOverlayTrackingNative]::SetForegroundWindow($source.Hwnd)
  Start-Sleep -Milliseconds 100
  [void](Add-TrackingSample 'baseline' 0 $sourceVisibleBefore)

  # Real 1-4px moves: the indicator must not inherit a deadband from the
  # telepointer normalization loop.
  foreach ($delta in @(1, 2, 3, 4)) {
    Invoke-SourceAction "move-${delta}px" {
      $current = (Get-WindowRecord $source.Hwnd).Frame
      Set-SourcePosition $source ($current.X + $delta) ($current.Y + $delta) $current.Width $current.Height
    } $true
  }

  Invoke-ContinuousMove $source

  Invoke-SourceAction 'maximize' {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_MAXIMIZE)
  } $true
  Invoke-SourceAction 'restore-after-maximize' {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_RESTORE)
  } $true

  Invoke-SourceAction 'minimize' {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_MINIMIZE)
  } $false
  Invoke-SourceAction 'restore-after-minimize' {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_RESTORE)
    [void][PetalShareOverlayTrackingNative]::SetForegroundWindow($source.Hwnd)
  } $true

  # Foreground/title-bar activation is repeated without changing the source
  # geometry. Any ownership or visibility bounce here is a visible flash.
  for ($i = 1; $i -le 5; $i++) {
    Invoke-SourceAction "titlebar-activation-$i" {
      [void][PetalShareOverlayTrackingNative]::SetForegroundWindow($source.Hwnd)
      Send-TitleBarClick $source
      [void][PetalShareOverlayTrackingNative]::SetForegroundWindow($source.Hwnd)
    } $true
  }

  $occluderCheck = $null
  if ($null -ne $occluder) {
    if (-not [PetalShareOverlayTrackingNative]::SetForegroundWindow($occluder.Hwnd)) {
      throw "Could not foreground occluder '$($occluder.Title)'."
    }
    Start-Sleep -Milliseconds 100
    $sourceFrame = Get-VisibleWindowFrame $source.Hwnd
    $occluderFrame = Get-VisibleWindowFrame $occluder.Hwnd
    $occluderCheck = [pscustomobject]@{
      Overlap = Test-RectsOverlap $occluderFrame $sourceFrame
      AboveSource = Test-WindowAbove $occluder.Hwnd $source.Hwnd
      AboveOverlay = Test-WindowAbove $occluder.Hwnd $overlay.Hwnd
    }
    [void][PetalShareOverlayTrackingNative]::SetForegroundWindow($source.Hwnd)
  }
} finally {
  # Restore the user's source window and cursor even when a red assertion
  # stops the exercise.
  if ($sourceWasMinimized) {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_MINIMIZE)
  } else {
    [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_RESTORE)
    Set-SourcePosition $source $sourceOuterBefore.X $sourceOuterBefore.Y $sourceOuterBefore.Width $sourceOuterBefore.Height
    if ($sourceWasMaximized) { [void][PetalShareOverlayTrackingNative]::ShowWindow($source.Hwnd, $SW_MAXIMIZE) }
  }
  if ($null -ne $cursorBefore) { [void][PetalShareOverlayTrackingNative]::SetCursorPos($cursorBefore.X, $cursorBefore.Y) }
}

$firstPaintAfter = Get-LogFirstPaintCount $logPath
$visibleSamples = @($samples | Where-Object { $_.SourceVisible })
$latencies = @($actions | Where-Object { $null -ne $_.LatencyMs } | ForEach-Object { [double]$_.LatencyMs } | Sort-Object)
function Get-Percentile {
  param([double[]]$Values, [double]$Percentile)
  if ($Values.Count -eq 0) { return $null }
  $index = [int][Math]::Ceiling($Values.Count * $Percentile) - 1
  $index = [Math]::Max(0, [Math]::Min($index, $Values.Count - 1))
  [math]::Round($Values[$index], 1)
}
$maxError = @($visibleSamples | Where-Object { $null -ne $_.EdgeErrorPx } | Measure-Object EdgeErrorPx -Maximum).Maximum
if ($null -eq $maxError) { $maxError = 0 }
$visibilityGaps = @($visibleSamples | Where-Object { -not $_.OverlayVisible }).Count
$ownerGaps = if ($Mode -eq 'Owned') {
  @($visibleSamples | Where-Object { -not $_.OwnerOkay }).Count
} else { 0 }
$stackingGaps = if ($Mode -eq 'Owned') {
  @($visibleSamples | Where-Object { -not $_.StackingOkay }).Count
} else { 0 }
$hybrids = @($visibleSamples | Where-Object { $_.Hybrid }).Count
$unsettled = @($actions | Where-Object { -not $_.Settled })
$firstPaintDelta = $firstPaintAfter - $firstPaintBefore
$logTail = Get-LogTail $logPath
$systemIndicatorLogSeen = @($logTail | Where-Object { $_ -match 'indicator mode=System' }).Count -gt 0
$passiveCustomReadinessSeen = @($logTail | Where-Object { $_ -match 'readiness .*custom=true' }).Count -gt 0

if ($CsvPath) { $samples | Export-Csv -LiteralPath $CsvPath -NoTypeInformation }

Write-Output "Mode=$Mode"
Write-Output "Source=$($source.Title) HWND=0x$($source.Hwnd.ToInt64().ToString('X'))"
Write-Output "Overlay=$overlayTitle HWND=0x$($overlay.Hwnd.ToInt64().ToString('X'))"
Write-Output "Samples=$($samples.Count) SampleMs=$SampleMilliseconds"
Write-Output ("ConvergenceLatencyMs p50={0} p95={1} max={2}" -f (Get-Percentile $latencies 0.50), (Get-Percentile $latencies 0.95), (if ($latencies.Count -eq 0) { $null } else { $latencies[-1] }))
Write-Output ("MaxEdgeErrorPx={0} VisibilityGaps={1} OwnerGaps={2} StackingGaps={3} HybridRectangles={4}" -f ([math]::Round([double]$maxError, 1)), $visibilityGaps, $ownerGaps, $stackingGaps, $hybrids)
if ($null -ne $occluderCheck) {
  Write-Output ("OccluderOverlap={0} AboveSource={1} AboveOverlay={2}" -f $occluderCheck.Overlap, $occluderCheck.AboveSource, $occluderCheck.AboveOverlay)
}
Write-Output "FrontendFirstPaintDelta=$firstPaintDelta (before=$firstPaintBefore after=$firstPaintAfter)"
Write-Output "SystemIndicatorLogSeen=$systemIndicatorLogSeen PassiveCustomReadinessSeen=$passiveCustomReadinessSeen"
Write-Output '--- action results ---'
$actions | Format-Table -AutoSize | Out-String -Width 180 | Write-Output
Write-Output '--- bounded Petal overlay log tail ---'
if (Test-Path -LiteralPath $logPath) {
  Get-Content -LiteralPath $logPath -Tail 160 | Where-Object {
    $_ -match 'share overlay|indicator mode|setup complete|first frame|capture session torn down|frontend first paint'
  }
}
Write-Output '--- end bounded Petal overlay log tail ---'

$redReasons = New-Object 'System.Collections.Generic.List[string]'
if ([double]$maxError -gt $SettledEdgeTolerancePixels) { $redReasons.Add("settled edge error exceeded ${SettledEdgeTolerancePixels}px") }
if ($visibilityGaps -gt 0) { $redReasons.Add("$visibilityGaps visibility gap samples") }
if ($ownerGaps -gt 0) { $redReasons.Add("$ownerGaps ownership gap samples") }
if ($stackingGaps -gt 0) { $redReasons.Add("$stackingGaps stacking gap samples") }
if ($Mode -eq 'Passive' -and -not $systemIndicatorLogSeen) {
  $redReasons.Add('passive mode did not find an indicator mode=System log entry')
}
if ($Mode -eq 'Passive' -and $passiveCustomReadinessSeen) {
  $redReasons.Add('passive mode found a custom=true readiness log entry')
}
if ($null -ne $occluderCheck -and (-not $occluderCheck.Overlap -or -not $occluderCheck.AboveSource -or -not $occluderCheck.AboveOverlay)) {
  $redReasons.Add('overlapping occluder was not above both source and overlay')
}
if ($hybrids -gt 0) { $redReasons.Add("$hybrids hybrid rectangle samples") }
if ($firstPaintDelta -ne 0) { $redReasons.Add("frontend first-paint count changed by $firstPaintDelta") }
if ($unsettled.Count -gt 0) { $redReasons.Add("unsettled actions: $($unsettled.Name -join ', ')") }
if ($redReasons.Count -gt 0) {
  Write-Error ("RED: " + ($redReasons -join '; '))
  exit 2
}
Write-Output "PASS: $Mode overlay tracked the visible source frame without gaps, hybrids, or reloads."
