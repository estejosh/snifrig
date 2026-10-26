# snifrig's administrator helpers (run once as administrator; Windows asks you first).
# They live in C:\Program Files\snifrig, where only administrators can change files, so the
# scheduled tasks that run them with full rights cannot be swapped for something else by a
# program running as you. Their output goes to C:\Program Files\snifrig\state (you can read it).
#   -Want frames,drivers   which helpers to set up; any left out are removed.
param([string]$Src, [string]$Dir, [string]$Want, [string]$Out)
$ErrorActionPreference = 'Stop'
try {
  $root = Join-Path $env:ProgramFiles 'snifrig'
  $helpers = Join-Path $root 'helpers'
  $state = Join-Path $root 'state'
  New-Item -ItemType Directory -Force $helpers, $state | Out-Null
  $w = @($Want -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
  $me = "$env:USERDOMAIN\$env:USERNAME"
  $p = New-ScheduledTaskPrincipal -UserId $me -LogonType Interactive -RunLevel Highest
  if ($w -contains 'frames') {
    Stop-ScheduledTask -TaskName 'snifrig frames' -ErrorAction SilentlyContinue
    Get-Process snifrig-frames -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$helpers\*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Copy-Item -Force (Join-Path $Src 'snifrig-frames.exe') $helpers
    $a = New-ScheduledTaskAction -Execute (Join-Path $helpers 'snifrig-frames.exe') -Argument "--dir `"$Dir`""
    $t = New-ScheduledTaskTrigger -AtLogOn -User $me
    $s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'snifrig frames' -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null
    Start-ScheduledTask -TaskName 'snifrig frames'
  } else {
    Stop-ScheduledTask -TaskName 'snifrig frames' -ErrorAction SilentlyContinue
    Get-Process snifrig-frames -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$helpers\*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName 'snifrig frames' -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item -Force (Join-Path $helpers 'snifrig-frames.exe') -ErrorAction SilentlyContinue
  }
  if ($w -contains 'drivers') {
    Copy-Item -Force (Join-Path $Src 'drivers-install.ps1') $helpers
    $script = Join-Path $helpers 'drivers-install.ps1'
    $a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$script`" -Dir `"$Dir`""
    $s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Hours 2) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName 'snifrig drivers' -Action $a -Principal $p -Settings $s -Force | Out-Null
  } else {
    Unregister-ScheduledTask -TaskName 'snifrig drivers' -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item -Force (Join-Path $helpers 'drivers-install.ps1') -ErrorAction SilentlyContinue
  }
  'ok' | Out-File -Encoding utf8 $Out
} catch { "failed: $($_.Exception.Message)" | Out-File -Encoding utf8 $Out }
