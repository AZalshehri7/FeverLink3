<#
.SYNOPSIS
  The whole local check in one command: every suite CI runs, one summary table, exit code 0 only when all passed.

.DESCRIPTION
  Runs, in this order, and goes on after a failure so the table shows every suite:
    fmt      cargo fmt --all --check
    clippy   cargo clippy --workspace --all-targets -- -D warnings
    core     cargo test --workspace --exclude feverlink3_native        (the core and the launcher)
    native   cargo test -p feverlink3_native -- --test-threads=1        (process-wide hook state: one thread)
    lua      python -I tests/lua/run_parallel.py                         (the lupa suite on every core)
  -Full adds the ignored tests (--include-ignored) and FEVERLINK3_FULL_TESTS=1, and runs the lupa suite in ONE process
  (python -I -m unittest discover): the full set has tests with tight wall-clock bounds that a fully loaded PC can miss
  (docs/RELEASING.md).
  -Only fmt,lua (a comma list) runs those suites alone. Each suite's output goes to <-LogDir>\<suite>.log (default: a new
  folder under %TEMP%); a failed suite's last 40 lines are printed under the table.

  powershell -NoProfile -ExecutionPolicy Bypass -File tools\check.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File tools\check.ps1 -Full
  Exit codes: 0 every suite passed, 1 a suite failed, 2 bad parameters or a tool (cargo, python) missing.
  Nothing here starts a game, touches the game folder or the network beyond what cargo needs to fetch crates.
#>
param(
    [switch]$Full,
    [string]$Only = '',
    [string]$LogDir = ''
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$all = @('fmt', 'clippy', 'core', 'native', 'lua')
$pick = @($all)
if ($Only) {
    $pick = @($Only -split '[,\s]+' | Where-Object { $_ })
    $bad = @($pick | Where-Object { $all -notcontains $_ })
    if ($bad.Count) { Write-Host ("unknown suite(s): {0} (known: {1})" -f ($bad -join ', '), ($all -join ', ')); exit 2 }
}
foreach ($tool in 'cargo', 'python') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Write-Host "$tool is not on PATH"; exit 2 }
}
if (-not $LogDir) { $LogDir = Join-Path $env:TEMP ('fl3-check-' + (Get-Date).ToString('yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force $LogDir | Out-Null

$ign = @(); if ($Full) { $ign = @('--include-ignored') }
$suites = [ordered]@{
    fmt    = @('cargo', @('fmt', '--all', '--check'))
    clippy = @('cargo', @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings'))
    core   = @('cargo', (@('test', '--workspace', '--exclude', 'feverlink3_native', '--') + $ign))
    native = @('cargo', (@('test', '-p', 'feverlink3_native', '--', '--test-threads=1') + $ign))
    lua    = $(if ($Full) { @('python', @('-I', '-m', 'unittest', 'discover', '-s', 'tests/lua', '-p', 'test_*.py')) } else { @('python', @('-I', 'tests/lua/run_parallel.py')) })
}

$oldFull = $env:FEVERLINK3_FULL_TESTS
if ($Full) { $env:FEVERLINK3_FULL_TESTS = '1' }
$rows = @()
$total = [Diagnostics.Stopwatch]::StartNew()
Push-Location $repo
try {
    foreach ($name in $suites.Keys) {
        if ($pick -notcontains $name) { continue }
        $exe = $suites[$name][0]; $args1 = $suites[$name][1]
        $log = Join-Path $LogDir ($name + '.log')
        Write-Host ("{0,-7} {1} {2}" -f $name, $exe, ($args1 -join ' '))
        $sw = [Diagnostics.Stopwatch]::StartNew()
        # cmd /c so the native tool's stderr goes to the log as text (PowerShell 5.1 would wrap each stderr line in an error)
        $line = ('"{0}" {1} > "{2}" 2>&1' -f $exe, (($args1 | ForEach-Object { if ($_ -match '[\s*]') { '"' + $_ + '"' } else { $_ } }) -join ' '), $log)
        & cmd.exe /d /c $line
        $code = $LASTEXITCODE
        $sw.Stop()
        $text = ''; try { $text = [IO.File]::ReadAllText($log) } catch { }
        # what passed, from the suite's own summary lines (cargo: every "test result"; lua: the runner's or unittest's "Ran")
        $note = ''
        if ($exe -eq 'cargo' -and $name -ne 'fmt' -and $name -ne 'clippy') {
            $p = 0; $f = 0; $i = 0
            foreach ($m in [regex]::Matches($text, 'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored')) { $p += [int]$m.Groups[1].Value; $f += [int]$m.Groups[2].Value; $i += [int]$m.Groups[3].Value }
            $note = "$p passed, $f failed, $i ignored"
            if ($code -eq 0 -and $p -eq 0) { $code = 1; $note += ' (NO TEST RAN)' }
        } elseif ($name -eq 'lua') {
            $m = [regex]::Match($text, '(?m)^Ran (\d+)( of \d+)? tests? in [^\r\n]*')
            if ($m.Success) { $note = $m.Value } else { $note = 'no "Ran" line' }
            if ($code -eq 0 -and (-not $m.Success -or [int]$m.Groups[1].Value -eq 0)) { $code = 1; $note += ' (NO TEST RAN)' }
        }
        $rows += [pscustomobject]@{ Suite = $name; Status = $(if ($code -eq 0) { 'PASS' } else { 'FAIL' }); Secs = [Math]::Round($sw.Elapsed.TotalSeconds, 1); Note = $note; Log = $log; Code = $code }
    }
} finally {
    Pop-Location
    $env:FEVERLINK3_FULL_TESTS = $oldFull
}
$total.Stop()

Write-Host ''
Write-Host ("{0,-7} {1,-5} {2,7}  {3}" -f 'SUITE', 'STATUS', 'SECS', 'DETAIL')
foreach ($r in $rows) { Write-Host ("{0,-7} {1,-5} {2,7}  {3}" -f $r.Suite, $r.Status, $r.Secs, $r.Note) }
Write-Host ("total {0:N1} s{1}; logs in {2}" -f $total.Elapsed.TotalSeconds, $(if ($Full) { ' (full set)' } else { '' }), $LogDir)
$failed = @($rows | Where-Object { $_.Status -ne 'PASS' })
foreach ($r in $failed) {
    Write-Host ''
    Write-Host ("---- {0} failed (exit {1}), last lines of {2}:" -f $r.Suite, $r.Code, $r.Log)
    try { Get-Content $r.Log -Tail 40 | ForEach-Object { Write-Host $_ } } catch { }
}
if ($failed.Count) { Write-Host ("FAILED: {0}" -f (($failed | ForEach-Object { $_.Suite }) -join ', ')); exit 1 }
Write-Host 'ALL PASSED'
exit 0
