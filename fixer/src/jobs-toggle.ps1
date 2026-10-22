# Turn Windows scheduled tasks off or back on, one full task path per line in -ListFile.
# Run elevated by snifrig when a task belongs to an administrator (Windows asks you first).
param([string]$Mode, [string]$ListFile)
foreach ($full in Get-Content $ListFile | Where-Object { $_.Trim() }) {
  $i = $full.LastIndexOf('\')
  $path = $full.Substring(0, $i + 1); $name = $full.Substring($i + 1)
  try {
    if ($Mode -eq 'enable') { Enable-ScheduledTask -TaskPath $path -TaskName $name -ErrorAction Stop | Out-Null }
    else { Disable-ScheduledTask -TaskPath $path -TaskName $name -ErrorAction Stop | Out-Null }
    "ok: $full"
  } catch { "failed: $full ($($_.Exception.Message))" }
}
