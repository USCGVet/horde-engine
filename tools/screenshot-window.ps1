# Saves a PNG of a process's main window, even when other windows cover it, using PrintWindow with full
# GPU content. Handy for checking the client's rendering from a script.
# usage: pwsh tools/screenshot-window.ps1 <process name> <output.png>
param([Parameter(Mandatory)][string]$Process, [Parameter(Mandatory)][string]$Out)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace Native -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
public struct RECT { public int Left, Top, Right, Bottom; }
'@

# Without this, display scaling makes window sizes come back in scaled units and the capture gets cropped.
[void][Native.Win]::SetProcessDPIAware()
$p = Get-Process -Name $Process | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1
if (-not $p) { throw "no window for process '$Process'" }
$rect = New-Object Native.Win+RECT
[void][Native.Win]::GetWindowRect($p.MainWindowHandle, [ref]$rect)
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[void][Native.Win]::PrintWindow($p.MainWindowHandle, $hdc, 2)  # PW_RENDERFULLCONTENT
$g.ReleaseHdc($hdc)
$g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "saved ${w}x${h} to $Out ($($p.MainWindowTitle))"
