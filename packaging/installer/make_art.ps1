<#
.SYNOPSIS
    Regenerates the Inno Setup wizard artwork for the Thrennix installer.

.DESCRIPTION
    Writes 24-bit BMPs next to this script:
      wizard-large-100.bmp .. wizard-large-200.bmp  (164x314, 205x388, 246x459, 328x628)
      wizard-small-100.bmp .. wizard-small-200.bmp  (55x55, 69x69, 83x83, 110x110)
    Everything is drawn as vectors with System.Drawing, so each size is crisp.

    Compatible with Windows PowerShell 5.1 and PowerShell 7+ on Windows.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\installer\make_art.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
if (-not $OutDir) { $OutDir = Split-Path -Parent $MyInvocation.MyCommand.Path }
Add-Type -AssemblyName System.Drawing

$Deep = [System.Drawing.Color]::FromArgb(255, 0x0b, 0x3d, 0x91)
$Brand = [System.Drawing.Color]::FromArgb(255, 0x1a, 0x73, 0xe8)
$White = [System.Drawing.Color]::White

function New-RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

# The Thrennix mark: a down-arrow over a tray line, centred in a square of side $s at ($x,$y).
function Add-ArrowMark($g, [single]$x, [single]$y, [single]$s, [System.Drawing.Color]$color) {
    $brush = New-Object System.Drawing.SolidBrush($color)
    $cx = $x + $s / 2
    # Shaft
    $shaftW = $s * 0.14
    $shaftTop = $y + $s * 0.20
    $shaftBottom = $y + $s * 0.52
    $shaft = New-RoundRect ($cx - $shaftW / 2) $shaftTop $shaftW ($shaftBottom - $shaftTop + 1) ($shaftW * 0.35)
    $g.FillPath($brush, $shaft)
    # Head
    $headW = $s * 0.46
    $headTop = $y + $s * 0.44
    $headTip = $y + $s * 0.70
    $pts = [System.Drawing.PointF[]]@(
        (New-Object System.Drawing.PointF(($cx - $headW / 2), $headTop)),
        (New-Object System.Drawing.PointF(($cx + $headW / 2), $headTop)),
        (New-Object System.Drawing.PointF($cx, $headTip))
    )
    $head = New-Object System.Drawing.Drawing2D.GraphicsPath
    $head.AddPolygon($pts)
    $g.FillPath($brush, $head)
    # Tray
    $trayW = $s * 0.56
    $trayH = $s * 0.085
    $tray = New-RoundRect ($cx - $trayW / 2) ($y + $s * 0.76) $trayW $trayH ($trayH / 2)
    $g.FillPath($brush, $tray)
    $brush.Dispose()
}

function New-Canvas([int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    return @{ Bitmap = $bmp; Graphics = $g }
}

function Save-Bmp($canvas, [string]$name) {
    $path = Join-Path $OutDir $name
    $canvas.Graphics.Dispose()
    $canvas.Bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $canvas.Bitmap.Dispose()
    Write-Host "  $name"
}

function New-Font([string[]]$families, [single]$px) {
    foreach ($f in $families) {
        try {
            $font = New-Object System.Drawing.Font($f, $px, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
            if ($font.Name -eq $f) { return $font }
            $font.Dispose()
        }
        catch { }
    }
    return New-Object System.Drawing.Font('Segoe UI', $px, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
}

function Draw-CenteredText($g, [string]$text, $font, [System.Drawing.Color]$color, [single]$width, [single]$y) {
    $brush = New-Object System.Drawing.SolidBrush($color)
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = [System.Drawing.StringAlignment]::Center
    $rect = New-Object System.Drawing.RectangleF(0, $y, $width, ($font.Size * 1.6))
    $g.DrawString($text, $font, $brush, $rect, $fmt)
    $brush.Dispose()
    $fmt.Dispose()
}

# ------------------------------------------------------------ large (left panel)

function New-LargeImage([int]$w, [int]$h, [string]$name) {
    $c = New-Canvas $w $h
    $g = $c.Graphics
    $k = $w / 164.0

    # Vertical gradient, deep at the top, brand blue at the bottom.
    $rect = New-Object System.Drawing.Rectangle(0, 0, $w, $h)
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $Deep, $Brand, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillRectangle($grad, $rect)
    $grad.Dispose()

    # Soft glow behind the logo.
    $glowCx = $w / 2
    $glowCy = $h * 0.36
    $glowR = $w * 0.62
    $glow = New-Object System.Drawing.Drawing2D.GraphicsPath
    $glow.AddEllipse(($glowCx - $glowR), ($glowCy - $glowR), ($glowR * 2), ($glowR * 2))
    $pgb = New-Object System.Drawing.Drawing2D.PathGradientBrush($glow)
    $pgb.CenterColor = [System.Drawing.Color]::FromArgb(70, 120, 180, 255)
    $pgb.SurroundColors = [System.Drawing.Color[]]@([System.Drawing.Color]::FromArgb(0, 120, 180, 255))
    $g.FillPath($pgb, $glow)
    $pgb.Dispose()

    # Subtle diagonal speed-lines, rising to the right.
    $lines = @(
        @(0.05, 0.62, 0.55, 2.0, 22),
        @(0.20, 0.70, 0.75, 1.2, 16),
        @(-0.10, 0.80, 0.60, 3.0, 14),
        @(0.35, 0.58, 0.85, 1.0, 20),
        @(0.10, 0.90, 0.95, 1.6, 12),
        @(0.45, 0.13, 1.10, 1.4, 14),
        @(0.60, 0.22, 1.15, 2.4, 10)
    )
    foreach ($l in $lines) {
        $x0 = $l[0] * $w; $y0 = $l[1] * $h
        $len = $l[2] * $w
        $x1 = $x0 + $len; $y1 = $y0 - $len * 0.45
        $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb([int]$l[4], 255, 255, 255), [single]($l[3] * $k))
        $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $g.DrawLine($pen, [single]$x0, [single]$y0, [single]$x1, [single]$y1)
        $pen.Dispose()
    }

    # Logo tile: the app icon's blue rounded square (lighter top-left) with a white arrow.
    $tile = 76 * $k
    $tx = ($w - $tile) / 2
    $ty = $h * 0.36 - $tile / 2
    $tilePath = New-RoundRect $tx $ty $tile $tile ($tile * 0.24)
    # Soft drop shadow.
    $shadowPath = New-RoundRect $tx ($ty + 3 * $k) $tile $tile ($tile * 0.24)
    $shadowBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(60, 0, 20, 70))
    $g.FillPath($shadowBrush, $shadowPath)
    $shadowBrush.Dispose()
    $tileRect = New-Object System.Drawing.RectangleF($tx, $ty, $tile, $tile)
    $tileBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($tileRect,
        [System.Drawing.Color]::FromArgb(255, 0x4a, 0x94, 0xff), $Brand,
        [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal)
    $g.FillPath($tileBrush, $tilePath)
    $tileBrush.Dispose()
    $tilePen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(120, 255, 255, 255), [single](1.2 * $k))
    $g.DrawPath($tilePen, $tilePath)
    $tilePen.Dispose()
    Add-ArrowMark $g $tx $ty $tile $White

    # Wordmark and byline near the bottom.
    $titleFont = New-Font @('Segoe UI Semibold', 'Segoe UI') (25 * $k)
    $byFont = New-Font @('Segoe UI') (11 * $k)
    Draw-CenteredText $g 'Thrennix' $titleFont $White $w ($h - 92 * $k)
    Draw-CenteredText $g 'by PNT Apps' $byFont ([System.Drawing.Color]::FromArgb(255, 205, 225, 255)) $w ($h - 52 * $k)
    $titleFont.Dispose()
    $byFont.Dispose()

    # Thin accent rule between wordmark and byline.
    $rulePen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(110, 255, 255, 255), [single](1 * $k))
    $g.DrawLine($rulePen, [single]($w / 2 - 14 * $k), [single]($h - 56 * $k), [single]($w / 2 + 14 * $k), [single]($h - 56 * $k))
    $rulePen.Dispose()

    Save-Bmp $c $name
}

# ------------------------------------------------------------ small (top-right)

function New-SmallImage([int]$s, [string]$name) {
    $c = New-Canvas $s $s
    $g = $c.Graphics
    $g.Clear($White)
    $pad = $s * 0.06
    $side = $s - 2 * $pad
    $tilePath = New-RoundRect $pad $pad $side $side ($side * 0.22)
    $rect = New-Object System.Drawing.RectangleF($pad, $pad, $side, $side)
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $Brand, $Deep, [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal)
    $g.FillPath($grad, $tilePath)
    $grad.Dispose()
    Add-ArrowMark $g $pad $pad $side $White
    Save-Bmp $c $name
}

Write-Host "Writing installer artwork to $OutDir"
$large = @(@(164, 314, 100), @(205, 388, 125), @(246, 459, 150), @(328, 628, 200))
foreach ($l in $large) { New-LargeImage $l[0] $l[1] ("wizard-large-{0}.bmp" -f $l[2]) }
$small = @(@(55, 100), @(69, 125), @(83, 150), @(110, 200))
foreach ($s in $small) { New-SmallImage $s[0] ("wizard-small-{0}.bmp" -f $s[1]) }
