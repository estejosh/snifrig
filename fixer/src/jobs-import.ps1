# snifrig jobs import: copies your script-type scheduled tasks into snifrig, backs each one up
# and turns the Windows copy off. Apps' own tasks (updaters, vendor tools) stay in Windows.
# Writes jobs-import.txt (job blocks) and jobs-import-denied.txt (tasks Windows would not let
# us turn off without your OK). Prints one line per task.
param([string]$Dir, [switch]$Preview, [string]$Only)
$ErrorActionPreference = 'Continue'
$hosts = 'powershell.exe','pwsh.exe','cmd.exe','python.exe','pythonw.exe','py.exe','wsl.exe','wscript.exe','cscript.exe','node.exe','uv.exe','bash.exe'
# Ferryman's own tasks (it manages and silences them itself) and never-touch programs stay in Windows.
$skip = 'ferryman','ferry-autoenable','\ferry\bin\','\ferry\service\','gpu_arbiter'
$bk = Join-Path $Dir 'task-backups'
New-Item -ItemType Directory -Force $bk | Out-Null
$out = New-Object System.Text.StringBuilder
$denied = @()
$names = @{}
$seed = New-Object System.Text.StringBuilder
foreach ($t in Get-ScheduledTask | Where-Object { $_.TaskPath -notlike '\Microsoft\*' -and $_.Settings.Enabled }) {
  $full = "$($t.TaskPath)$($t.TaskName)"
  if ($Only -and $full -ne $Only) { continue }
  $acts = @($t.Actions)
  if ($acts.Count -ne 1) { continue }
  $a = $acts[0]
  $exe = [Environment]::ExpandEnvironmentVariables(("$($a.Execute)" -replace '"', ''))
  $targs = "$($a.Arguments)"
  $line = "$full $exe $targs".ToLower()
  if ($skip | Where-Object { $line.Contains($_) }) { "skip (Ferryman runs its own tasks): $full"; continue }
  # Unwrap a conhost --headless wrapper (snifrig or Ferryman added it to hide the window).
  if ((Split-Path $exe -Leaf).ToLower() -eq 'conhost.exe' -and $targs -match '^\s*--headless\s+(.*)$') {
    $rest = $Matches[1].Trim()
    if ($rest.StartsWith('"')) { $i = $rest.IndexOf('"', 1); $exe = $rest.Substring(1, $i - 1); $targs = $rest.Substring($i + 1).Trim() }
    else { $sp = $rest.IndexOf(' '); if ($sp -lt 0) { $exe = $rest; $targs = '' } else { $exe = $rest.Substring(0, $sp); $targs = $rest.Substring($sp + 1).Trim() } }
  }
  $leaf = (Split-Path $exe -Leaf).ToLower()
  if ($leaf -notmatch '\.') { $leaf = "$leaf.exe" }
  $isScript = $leaf -match '\.(cmd|bat)$'
  if (-not $isScript -and $hosts -notcontains $leaf) { continue }
  # cmd /c already wrapping a script stays as it is
  $run = if ($isScript) { "cmd.exe /c `"$exe`" $targs".Trim() } else { "`"$exe`" $targs".Trim() }
  # A repeating trigger wins (a task can also have an at-logon trigger), then logon, then daily.
  $sched = $null
  foreach ($tr in $t.Triggers) { $iv = $tr.Repetition.Interval; if ($iv) { $ts = [System.Xml.XmlConvert]::ToTimeSpan($iv); $sched = "every $([int][Math]::Max(1, $ts.TotalMinutes))m"; break } }
  if (-not $sched) {
    foreach ($tr in $t.Triggers) {
      $cls = $tr.CimClass.CimClassName
      if ($cls -eq 'MSFT_TaskLogonTrigger' -or $cls -eq 'MSFT_TaskBootTrigger') { $sched = 'at logon'; break }
      if ($cls -eq 'MSFT_TaskDailyTrigger') { if ($tr.DaysInterval -gt 1) { $sched = "every $($tr.DaysInterval)d" } else { $sched = "daily {0:HH:mm}" -f ([datetime]$tr.StartBoundary) }; break }
      if ($cls -eq 'MSFT_TaskWeeklyTrigger') { $sched = 'every 7d'; break }
      if ($cls -eq 'MSFT_TaskTimeTrigger' -and $tr.StartBoundary) {
        # A one-time task: copy it if it has not run yet.
        $sb = [datetime]$tr.StartBoundary
        $lr = (Get-ScheduledTaskInfo -TaskName $t.TaskName -TaskPath $t.TaskPath -ErrorAction SilentlyContinue).LastRunTime
        if (-not $lr -or $lr -lt $sb) { $sched = "once $([DateTimeOffset]::new($sb).ToUnixTimeSeconds())" }
        break
      }
    }
  }
  if (-not $sched) { "skip (no schedule snifrig can copy): $full"; continue }
  $when = 'not gaming'
  if ($sched -match '^every (\d+)m$' -and [int]$Matches[1] -le 10) { $when = 'anytime' }
  if ($sched -eq 'at logon') { $when = 'anytime' }
  $lim = "$($t.Settings.ExecutionTimeLimit)"
  $max = 'none'
  if ($lim -and $lim -ne 'PT0S' -and $lim -ne 'PT72H') { $max = "$([int]([System.Xml.XmlConvert]::ToTimeSpan($lim)).TotalMinutes)m" }
  $name = $t.TaskName
  if ($names.ContainsKey($name.ToLower())) { $name = $full.TrimStart('\') }
  $names[$name.ToLower()] = 1
  $safe = $full -replace '[\\/:*?"<>|]', '_'
  if (-not $Preview) { Export-ScheduledTask -TaskName $t.TaskName -TaskPath $t.TaskPath | Out-File (Join-Path $bk "$safe.xml") -Encoding unicode }
  if (-not $Preview) { try { Disable-ScheduledTask -TaskName $t.TaskName -TaskPath $t.TaskPath -ErrorAction Stop | Out-Null }
  catch { $denied += $full } }
  [void]$out.AppendLine('')
  [void]$out.AppendLine("[$name]")
  [void]$out.AppendLine("run = $run")
  if ($a.WorkingDirectory) { [void]$out.AppendLine("dir = $($a.WorkingDirectory)") }
  [void]$out.AppendLine("schedule = $sched")
  [void]$out.AppendLine("when = $when")
  [void]$out.AppendLine("max = $max")
  [void]$out.AppendLine("from_task = $full")
  $lr = (Get-ScheduledTaskInfo -TaskName $t.TaskName -TaskPath $t.TaskPath -ErrorAction SilentlyContinue).LastRunTime
  if ($lr -and $lr.Year -gt 2000) { [void]$seed.AppendLine("$name`t$([DateTimeOffset]::new($lr).ToUnixTimeSeconds())") }
  "moved: $full ($sched, $when)"
}
$suffix = if ($Preview) { '-preview' } else { '' }
[IO.File]::WriteAllText((Join-Path $Dir "jobs-import$suffix.txt"), $out.ToString())
if (-not $Preview) { [IO.File]::AppendAllText((Join-Path $Dir 'jobs-seed.txt'), $seed.ToString()) }
[IO.File]::WriteAllText((Join-Path $Dir 'jobs-import-denied.txt'), ($denied -join "`r`n"))
