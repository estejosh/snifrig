# snifrig driver scan (read-only; no administrator rights needed).
# Devices with problems, the drivers installed, the driver updates manufacturers publish for
# this exact hardware through Windows Update, and the graphics driver version. Writes JSON.
param([string]$Out)
$ErrorActionPreference = 'SilentlyContinue'
$problems = @(Get-CimInstance Win32_PnPEntity | Where-Object { $_.ConfigManagerErrorCode -ne 0 -and $_.ConfigManagerErrorCode -ne 22 } | ForEach-Object {
  [pscustomobject]@{ name = "$($_.Name)"; code = [int]$_.ConfigManagerErrorCode; id = "$($_.PNPDeviceID)"; class = "$($_.PNPClass)" } })
$disabled = @(Get-CimInstance Win32_PnPEntity | Where-Object { $_.ConfigManagerErrorCode -eq 22 } | ForEach-Object { "$($_.Name)" })
$drivers = @(Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceName -and $_.DriverVersion -and $_.InfName -like 'oem*' } | ForEach-Object {
  [pscustomobject]@{ device = "$($_.DeviceName)"; version = "$($_.DriverVersion)"; date = $(if ($_.DriverDate) { $_.DriverDate.ToString('yyyy-MM-dd') } else { '' });
    provider = "$($_.DriverProviderName)"; maker = "$($_.Manufacturer)"; signed = [bool]$_.IsSigned; class = "$($_.DeviceClass)"; inf = "$($_.InfName)"; hwid = "$($_.HardWareID)" } })
$updates = @()
$wuerr = ''
try {
  $s = New-Object -ComObject Microsoft.Update.Session
  $r = $s.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Driver' and IsHidden=0")
  foreach ($u in $r.Updates) {
    $hw = "$($u.DriverHardwareID)"
    $now = $drivers | Where-Object { $_.hwid -eq $hw } | Select-Object -First 1
    $updates += [pscustomobject]@{ id = "$($u.Identity.UpdateID)"; title = "$($u.Title)"; maker = "$($u.DriverManufacturer)"; model = "$($u.DriverModel)"; class = "$($u.DriverClass)";
      date = $(if ($u.DriverVerDate) { ([datetime]$u.DriverVerDate).ToString('yyyy-MM-dd') } else { '' }); hwid = $hw; bytes = [int64]$u.MaxDownloadSize;
      installed_version = "$($now.version)"; installed_provider = "$($now.provider)"; installed_date = "$($now.date)" }
  }
} catch { $wuerr = "$($_.Exception.Message)" }
$gpu = "$(& nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>$null)".Trim()
$bb = Get-CimInstance Win32_BaseBoard | Select-Object -First 1
$cs = Get-CimInstance Win32_ComputerSystem
$board = "$($bb.Manufacturer) $($bb.Product)".Trim()
$pc = "$($cs.Manufacturer) $($cs.Model)".Trim()
# Other driver updaters on this PC: what they cost is shown so you can decide whether to keep them.
$others = @(Get-ScheduledTask -ErrorAction SilentlyContinue | Where-Object { $_.TaskName -match 'Driver ?Easy|DriverBooster|Driver Booster|DriverPack|Snappy Driver|DriverMax|Driver Genius|DriverFix|Driver Support' } | ForEach-Object {
  $i = $_ | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue
  [pscustomobject]@{ kind = 'task'; name = "$($_.TaskName)"; state = "$($_.State)"; last = $(if ($i.LastRunTime -and $i.LastRunTime.Year -gt 2000) { $i.LastRunTime.ToString('yyyy-MM-dd HH:mm') } else { '' }); triggers = "$(@($_.Triggers | ForEach-Object { $_.CimClass.CimClassName -replace 'MSFT_Task','' -replace 'Trigger','' }) -join ', ')" } })
$others += @(Get-CimInstance Win32_Service | Where-Object { $_.DisplayName -match 'Driver ?Easy|Driver Booster|DriverMax|Driver Genius|DriverFix|Driver Support' } | ForEach-Object {
  [pscustomobject]@{ kind = 'service'; name = "$($_.DisplayName)"; state = "$($_.State) ($($_.StartMode))"; last = ''; triggers = '' } })
$others += @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match 'DriverEasy|DriverBooster|DriverMax|DriverGenius|DriverFix' } | ForEach-Object {
  [pscustomobject]@{ kind = 'running'; name = "$($_.ProcessName)"; state = "$([math]::Round($_.WorkingSet64 / 1MB)) MB in use"; last = ''; triggers = '' } })
# One fact per line, fields separated by tabs (snifrig reads this; tabs and line breaks in names
# are replaced by spaces).
function C($v) { "$v" -replace "[`t`r`n]+", ' ' }
function Row([string]$kind, [object[]]$f) { $kind + "`t" + (($f | ForEach-Object { C $_ }) -join "`t") }
$lines = @("unix`t$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())", (Row 'gpu' @($gpu)), (Row 'board' @($board)), (Row 'pc' @($pc)), (Row 'wu_error' @($wuerr)))
$lines += $problems | ForEach-Object { Row 'problem' @($_.name, $_.code, $_.class, $_.id) }
$lines += $disabled | ForEach-Object { Row 'disabled' @($_) }
$lines += $updates | ForEach-Object { Row 'update' @($_.id, $_.title, $_.maker, $_.class, $_.date, $_.bytes, $_.installed_version, $_.installed_provider, $_.installed_date, $_.hwid) }
$lines += $others | ForEach-Object { Row 'other' @($_.kind, $_.name, $_.state, $_.last, $_.triggers) }
$lines += $drivers | ForEach-Object { Row 'driver' @($_.device, $_.version, $_.date, $_.provider, $_.class, $_.signed) }
[IO.File]::WriteAllLines("$Out.tmp", [string[]]$lines, (New-Object Text.UTF8Encoding $false))
Move-Item -Force "$Out.tmp" $Out