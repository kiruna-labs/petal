#Requires -Version 5.1
<##
.SYNOPSIS
  Compare the local Windows display-share indicator with a received frame.

.DESCRIPTION
  Run while a full-display share is active and a peer has exported the raw
  received video frame (not a screenshot containing the receiver's title bar).
  The script captures the same monitor locally, measures identity-colored edge
  pixels in both images, and prints the bounded share/indicator log tail.

  A passing run means the local monitor has the Petal border while the received
  frame does not. If the local edge is not identity-colored, inspect the log:
  the expected safe result is indicator mode=System with Windows' native border.

.EXAMPLE
  .\windows-display-indicator-smoke.ps1 -DisplayLeft 0 -DisplayTop 0 `
    -DisplayWidth 2560 -DisplayHeight 1440 `
    -ReceivedFramePath .\received-display.png
#>

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [int]$DisplayLeft,
  [Parameter(Mandatory = $true)] [int]$DisplayTop,
  [Parameter(Mandatory = $true)] [ValidateRange(1, 16384)] [int]$DisplayWidth,
  [Parameter(Mandatory = $true)] [ValidateRange(1, 16384)] [int]$DisplayHeight,
  [Parameter(Mandatory = $true)] [string]$ReceivedFramePath,
  [string]$IdentityColor = '#8fa6b8',
  [ValidateRange(1, 60)] [int]$TimeoutSeconds = 10,
  [string]$LocalScreenshotPath = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $LocalScreenshotPath) {
  $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
  $LocalScreenshotPath = Join-Path ([IO.Path]::GetTempPath()) "petal-display-indicator-$stamp.png"
}
$logPath = if ($env:APPDATA) {
  Join-Path $env:APPDATA 'Petal\logs\petal.log'
} else {
  Join-Path ([IO.Path]::GetTempPath()) 'Petal\logs\petal.log'
}
$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
while (-not (Test-Path -LiteralPath $ReceivedFramePath) -and [DateTime]::UtcNow -lt $deadline) {
  Start-Sleep -Milliseconds 250
}
if (-not (Test-Path -LiteralPath $ReceivedFramePath)) {
  throw "Received frame did not appear within $TimeoutSeconds seconds: $ReceivedFramePath"
}

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies ([System.Drawing.Image].Assembly.Location) -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;

public static class PetalDisplayIndicatorSmoke {
    public static void SaveRegion(string path, int left, int top, int width, int height) {
        using (var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb))
        using (var graphics = Graphics.FromImage(bitmap)) {
            graphics.CopyFromScreen(left, top, 0, 0, new Size(width, height), CopyPixelOperation.SourceCopy);
            bitmap.Save(path, ImageFormat.Png);
        }
    }

    public static double EdgeColorFraction(string path, int expectedWidth, int expectedHeight, int r, int g, int b) {
        using (var bitmap = new Bitmap(path)) {
            if (bitmap.Width != expectedWidth || bitmap.Height != expectedHeight) {
                throw new InvalidOperationException(
                    string.Format("frame dimensions {0}x{1} do not match expected {2}x{3}", bitmap.Width, bitmap.Height, expectedWidth, expectedHeight));
            }
            int band = Math.Min(8, Math.Max(1, Math.Min(bitmap.Width, bitmap.Height) / 32));
            long matches = 0;
            long samples = 0;
            for (int y = 0; y < bitmap.Height; y++) {
                for (int x = 0; x < bitmap.Width; x++) {
                    if (x >= band && x < bitmap.Width - band && y >= band && y < bitmap.Height - band) continue;
                    var pixel = bitmap.GetPixel(x, y);
                    samples++;
                    if (Math.Abs(pixel.R - r) <= 10 && Math.Abs(pixel.G - g) <= 10 && Math.Abs(pixel.B - b) <= 10) matches++;
                }
            }
            return samples == 0 ? 0.0 : (double)matches / samples;
        }
    }
}
'@

$color = [System.Drawing.ColorTranslator]::FromHtml($IdentityColor)
[PetalDisplayIndicatorSmoke]::SaveRegion(
  $LocalScreenshotPath,
  $DisplayLeft,
  $DisplayTop,
  $DisplayWidth,
  $DisplayHeight
)
$localFraction = [PetalDisplayIndicatorSmoke]::EdgeColorFraction(
  $LocalScreenshotPath,
  $DisplayWidth,
  $DisplayHeight,
  $color.R,
  $color.G,
  $color.B
)
$receivedFraction = [PetalDisplayIndicatorSmoke]::EdgeColorFraction(
  $ReceivedFramePath,
  $DisplayWidth,
  $DisplayHeight,
  $color.R,
  $color.G,
  $color.B
)

Write-Output "LocalScreenshot=$LocalScreenshotPath"
Write-Output "ReceivedFrame=$ReceivedFramePath"
Write-Output ("IdentityEdgeFraction local={0:P2} received={1:P2}" -f $localFraction, $receivedFraction)

if (Test-Path -LiteralPath $logPath) {
  Write-Output '--- bounded Petal display-share log tail ---'
  Get-Content -LiteralPath $logPath -Tail 160 | Where-Object {
    $_ -match 'borderless|indicator|share overlay|display affinity|capture exclusion|share token'
  }
  Write-Output '--- end bounded log tail ---'
}

if ($localFraction -lt 0.10) {
  throw "Local display did not show enough identity-colored edge pixels. Inspect the log for indicator mode=System and the Windows native fallback."
}
if ($receivedFraction -gt 0.02) {
  throw "Received display frame contains identity-colored edge pixels; the local Petal indicator was published into the display share."
}
Write-Output 'PASS: local Petal indicator present; received display frame edge is clean.'
