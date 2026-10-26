# snifrig driver install (runs as administrator from C:\Program Files\snifrig\helpers, started by
# the scheduled task "snifrig drivers"). Reads the update ids you approved from
# <dir>\drivers-plan.txt and installs only updates Windows Update offers right now for this
# hardware: nothing else can be installed through it. A restore point first, then one driver at a
# time; after each, the device is checked, and a device that now shows a problem gets its previous
# driver back at once.
# <dir>\drivers-rollback.txt (update ids) instead takes back drivers this script installed earlier,
# looked up in its own record, so only those can be removed this way.
# The log and record live in ..\state, where only administrators can write; files in your folder
# are only read here (snifrig removes them afterwards).
param([string]$Dir)
$ErrorActionPreference = 'Continue'
# Load modules only from folders administrators control (your Documents folder is searched first
# otherwise, and this runs with full rights).
$env:PSModulePath = "$PSHOME\Modules;$env:ProgramFiles\WindowsPowerShell\Modules"
Import-Module "$PSHOME\Modules\CimCmdlets" -ErrorAction SilentlyContinue
$state = Join-Path (Split-Path $PSScriptRoot -Parent) 'state'
New-Item -ItemType Directory -Force $state | Out-Null
$log = Join-Path $state 'drivers-install.log'
$record = Join-Path $state 'drivers-installed.txt'
function L([string]$m) { "$(Get-Date -Format s) $m" | Out-File -Append -Encoding utf8 $log }
function Ids([string]$name) { $p = Join-Path $Dir $name; if (Test-Path $p) { @(Get-Content $p | ForEach-Object { $_.Trim() } | Where-Object { $_ -match '^[0-9a-fA-F-]{36}$' }) } else { @() } }
function Done([string]$what) { "$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds()) $what" | Out-File -Encoding utf8 (Join-Path $state 'drivers-result.txt') }

$back = Ids 'drivers-rollback.txt'
if ($back.Count -gt 0) {
  $n = 0
  foreach ($line in @(Get-Content $record -ErrorAction SilentlyContinue)) {
    $f = $line -split "`t"
    if ($f.Count -ge 4 -and $back -contains $f[1] -and $f[3] -match '^oem\d+\.inf$') {
      & pnputil.exe /delete-driver $f[3] /uninstall /force | Out-Null
      & pnputil.exe /scan-devices | Out-Null
      L "took back $($f[3]) ($($f[4])); the device uses its previous driver again"; $n++
    }
  }
  Done "rolledback $n"; exit 0
}

$plan = Ids 'drivers-plan.txt'
if ($plan.Count -eq 0) { L 'nothing planned'; Done 'nothing'; exit 0 }
L "starting: $($plan.Count) driver update(s)"
try {
  Enable-ComputerRestore -Drive "$env:SystemDrive\" -ErrorAction SilentlyContinue
  Checkpoint-Computer -Description 'snifrig: before driver updates' -RestorePointType MODIFY_SETTINGS -ErrorAction Stop
  L 'restore point made'
} catch {
  # Windows allows one restore point a day by default; a recent one counts.
  $recent = Get-ComputerRestorePoint -ErrorAction SilentlyContinue | Where-Object { ([Management.ManagementDateTimeConverter]::ToDateTime($_.CreationTime)) -gt (Get-Date).AddHours(-24) }
  if ($recent) { L 'using the restore point from the last 24 hours' } else { L "no restore point, so nothing installed: $($_.Exception.Message)"; Done 'norestore'; exit 2 }
}
$s = New-Object -ComObject Microsoft.Update.Session
$r = $s.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Driver' and IsHidden=0")
$done = 0; $bad = 0; $reboot = $false
foreach ($u in $r.Updates) {
  $id = "$($u.Identity.UpdateID)"
  if ($plan -notcontains $id) { continue }
  $hw = "$($u.DriverHardwareID)"
  $before = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.HardWareID -eq $hw } | Select-Object -First 1
  if (-not $u.EulaAccepted) { L "skipped '$($u.Title)': it has its own license terms to accept, so it is left for you"; continue }
  $c = New-Object -ComObject Microsoft.Update.UpdateColl
  [void]$c.Add($u)
  $d = $s.CreateUpdateDownloader(); $d.Updates = $c; [void]$d.Download()
  $i = $s.CreateUpdateInstaller(); $i.Updates = $c
  $res = $i.Install()
  if ($res.RebootRequired) { $reboot = $true }
  L "installed '$($u.Title)': result $($res.ResultCode) (2 = ok), restart needed: $($res.RebootRequired)"
  Start-Sleep -Seconds 15
  $after = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.HardWareID -eq $hw } | Select-Object -First 1
  $dev = Get-CimInstance Win32_PnPEntity | Where-Object { $_.HardwareID -contains $hw } | Select-Object -First 1
  $changed = $after -and $before -and $after.InfName -ne $before.InfName
  if ($changed) { "$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())`t$id`t$hw`t$($after.InfName)`t$($u.Title)`t$($before.DriverVersion)" | Out-File -Append -Encoding utf8 $record }
  if ($dev -and $dev.ConfigManagerErrorCode -ne 0 -and $changed) {
    L "the device now reports problem $($dev.ConfigManagerErrorCode): putting the previous driver ($($before.DriverVersion)) back"
    & pnputil.exe /delete-driver $after.InfName /uninstall /force | Out-Null
    & pnputil.exe /scan-devices | Out-Null
    L "rolled back $($u.Title)"; $bad++
  } elseif ($res.ResultCode -eq 2) { $done++ }
}
L "finished: $done driver(s) updated, $bad rolled back"
Done "installed $done rolledback $bad reboot $reboot"
