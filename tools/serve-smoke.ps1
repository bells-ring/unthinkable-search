# serve-smoke.ps1 — protocol test for `usc serve` over stdin/stdout and over a
# named pipe. Uses --live off (no UAC prompt): needs an existing index.
#   powershell -ExecutionPolicy Bypass -File tools\serve-smoke.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$usc = Join-Path $root 'target\release\usc.exe'
$fail = 0
function Check([bool]$cond, [string]$what) {
    if ($cond) { Write-Host "PASS  $what" } else { Write-Host "FAIL  $what"; $script:fail++ }
}

# ---------------------------------------------------------------- stdio
$psi = New-Object Diagnostics.ProcessStartInfo $usc, 'serve --live off'
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.StandardOutputEncoding = [Text.Encoding]::UTF8
$p = [Diagnostics.Process]::Start($psi)
$utf8 = New-Object Text.UTF8Encoding $false
$in = New-Object IO.StreamWriter($p.StandardInput.BaseStream, $utf8)
$in.AutoFlush = $true
function Req([string]$json) { $in.WriteLine($json); return ($p.StandardOutput.ReadLine() | ConvertFrom-Json) }

$hello = $p.StandardOutput.ReadLine() | ConvertFrom-Json
Check ($hello.event -eq 'hello' -and $hello.protocol -eq 1) "stdio hello: $($hello.version)"
$r = Req '{"id":1,"cmd":"ping"}'
Check ($r.ok -and $r.id -eq 1) 'ping'
$sw = [Diagnostics.Stopwatch]::StartNew()
$r = Req '{"id":"a","cmd":"search","query":"notepad.exe","sort":"size","desc":true,"limit":3,"fields":["full","size","mtime","ctime","dir"]}'
$rt = $sw.Elapsed.TotalMilliseconds
Check ($r.ok -and $r.id -eq 'a' -and $r.total -gt 0 -and $r.count -le 3) ("search: total {0}, engine {1} ms, round trip {2:N2} ms" -f $r.total, $r.ms, $rt)
Check ($r.items[0].size -ge $r.items[-1].size) 'sorted by size desc'
Check ($r.items[0].mtime -match '^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$') "iso time: $($r.items[0].mtime)"
$sw.Restart()
for ($i = 0; $i -lt 100; $i++) { $null = Req '{"cmd":"search","query":"dll","limit":10,"fields":"full"}' }
Write-Host ("      100 searches 'dll' (limit 10): {0:N2} ms avg round trip" -f ($sw.Elapsed.TotalMilliseconds / 100))
$r = Req '{"cmd":"search","query":"кириллица_несуществующее_имя"}'
Check ($r.ok -and $r.total -eq 0) 'unicode query, no results'
$r = Req '{"cmd":"search","query":"exe","offset":5,"limit":2,"time":"filetime","fields":"name,mtime"}'
Check ($r.ok -and $r.offset -eq 5 -and $r.count -eq 2 -and $r.items[0].mtime -is [long]) 'offset/limit/filetime'
$r = Req '{"id":7,"cmd":"info"}'
Check ($r.ok -and $r.entries -gt 0 -and $r.state -eq 'ready') "info: $($r.entries) entries, live=$($r.live), $($r.status)"
$r = Req 'not json'
Check (-not $r.ok -and $r.error -eq 'bad_json') 'bad json → error'
$r = Req '{"id":9,"cmd":"search"}'
Check (-not $r.ok -and $r.error -eq 'bad_request' -and $r.id -eq 9) 'missing query → bad_request'
$r = Req '{"cmd":"search","query":"x","sort":"nope"}'
Check (-not $r.ok) 'bad sort → error'
$r = Req '{"cmd":"frobnicate"}'
Check ($r.error -eq 'unknown_cmd') 'unknown cmd'
$r = Req '{"cmd":"subscribe"}'
Check $r.ok 'subscribe'
$r = Req '{"cmd":"quit"}'
Check $r.ok 'quit'
Check ($p.WaitForExit(10000) -and $p.ExitCode -eq 0) "stdio server exited (code $($p.ExitCode))"

# ---------------------------------------------------------------- named pipe
$name = 'us-smoke-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$psi2 = New-Object Diagnostics.ProcessStartInfo $usc, "serve --live off --pipe $name"
$psi2.UseShellExecute = $false
$psi2.RedirectStandardError = $true
$psi2.RedirectStandardOutput = $true
$s = [Diagnostics.Process]::Start($psi2)
function PipeClient {
    $c = New-Object IO.Pipes.NamedPipeClientStream('.', $name, [IO.Pipes.PipeDirection]::InOut)
    $c.Connect(10000)
    $r = New-Object IO.StreamReader($c, $utf8)
    $w = New-Object IO.StreamWriter($c, $utf8)
    $w.AutoFlush = $true
    [pscustomobject]@{ Pipe = $c; R = $r; W = $w }
}
function PReq($cl, [string]$json) { $cl.W.WriteLine($json); return ($cl.R.ReadLine() | ConvertFrom-Json) }
$a = PipeClient
$b = PipeClient
Check (($a.R.ReadLine() | ConvertFrom-Json).event -eq 'hello') 'pipe client A hello'
Check (($b.R.ReadLine() | ConvertFrom-Json).event -eq 'hello') 'pipe client B hello'
$ra = PReq $a '{"id":1,"cmd":"search","query":"notepad.exe","limit":1}'
$rb = PReq $b '{"id":2,"cmd":"search","query":"notepad.exe","limit":1}'
Check ($ra.ok -and $rb.ok -and $ra.total -eq $rb.total) "two concurrent pipe clients (total $($ra.total))"
$a.W.WriteLine('{"cmd":"quit"}'); $null = $a.R.ReadLine(); $a.Pipe.Dispose()
$rb = PReq $b '{"cmd":"ping"}'
Check $rb.ok 'client B still served after A quit'
$rb = PReq $b '{"cmd":"shutdown"}'
Check $rb.ok 'shutdown'
$b.Pipe.Dispose()
Check ($s.WaitForExit(10000) -and $s.ExitCode -eq 0) "pipe server exited (code $($s.ExitCode))"

if ($fail) { Write-Host "$fail FAILED"; exit 1 } else { Write-Host 'ALL PASSED' }
