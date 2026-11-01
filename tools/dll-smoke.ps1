# dll-smoke.ps1 — test unthinkable_search.dll through the C# example wrapper
# (examples\csharp\UnthinkableSearch.cs). No live updates → no UAC prompt;
# uses the existing index in target\release\data.
#   cargo build --profile release-dll -p us-ffi
#   powershell -ExecutionPolicy Bypass -File tools\dll-smoke.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$dll = Join-Path $root 'target\release-dll\unthinkable_search.dll'
$data = Join-Path $root 'target\release\data'
$fail = 0
function Check([bool]$cond, [string]$what) {
    if ($cond) { Write-Host "PASS  $what" } else { Write-Host "FAIL  $what"; $script:fail++ }
}

Add-Type -Path (Join-Path $root 'examples\csharp\UnthinkableSearch.cs')
[UnthinkableSearch.Engine]::LoadFrom($dll)
Check ([UnthinkableSearch.Native]::us_api_version() -eq 1) "api version 1, lib $([UnthinkableSearch.Engine]::Version)"

$e = [UnthinkableSearch.Engine]::Open($data, $false)
$sw = [Diagnostics.Stopwatch]::StartNew()
Check ($e.WaitReady(30000)) ("ready in {0} ms: {1} entries" -f $sw.ElapsedMilliseconds, $e.EntryCount)
Check ($e.Generation -ge 1) "generation $($e.Generation)"
Write-Host "      status: $($e.Status)"

$total = 0; $ms = 0.0
$items = $e.Search('notepad.exe', [UnthinkableSearch.Sort]::Size, $true, 5, [ref]$total, [ref]$ms)
Check ($total -gt 0 -and $items.Count -le 5) "search notepad.exe: $total results, $ms ms"
Check ($items[0].Size -ge $items[-1].Size) 'sorted by size desc'
Check ($items[0].FullPath -like '*\notepad.exe' -and $items[0].Name -eq 'notepad.exe') "item: $($items[0].FullPath)"
Check ($items[0].Modified -ne $null -and $items[0].Created -ne $null) "dates: modified $($items[0].Modified.ToString('u')), created $($items[0].Created.ToString('u'))"

$sw.Restart()
for ($i = 0; $i -lt 200; $i++) { $null = $e.Search('dll', [UnthinkableSearch.Sort]::Name, $false, 10) }
Write-Host ("      200 x search 'dll' + 10 items via P/Invoke: {0:N3} ms avg" -f ($sw.Elapsed.TotalMilliseconds / 200))

$dirs = $e.Search('folder:windows', [UnthinkableSearch.Sort]::Size, $true, 1)
Check ($dirs[0].IsDirectory -and $dirs[0].Size -gt 0) ("folder size: {0} = {1:N0} bytes" -f $dirs[0].FullPath, $dirs[0].Size)

$json = $e.Request('{"id":5,"cmd":"search","query":"кириллица_нет_такого|notepad.exe","limit":1,"fields":["full","size"]}')
$j = $json | ConvertFrom-Json
Check ($j.ok -and $j.id -eq 5 -and $j.items.Count -eq 1) "us_request json: total $($j.total)"
$j = $e.Request('{"cmd":"info"}') | ConvertFrom-Json
Check ($j.ok -and $j.entries -gt 0) "us_request info: $($j.entries) entries"

try { $null = $e.Search('x', ([Enum]::ToObject([UnthinkableSearch.Sort], 9)), $false, 1); Check $false 'bad sort rejected' }
catch { Check ($_.Exception.InnerException.Message -match 'bad sort' -or $_.Exception.Message -match 'bad sort') "bad sort → error: $($_.Exception.InnerException.Message)" }

$e.Dispose()
Check $true 'closed'
if ($fail) { Write-Host "$fail FAILED"; exit 1 } else { Write-Host 'ALL PASSED' }
