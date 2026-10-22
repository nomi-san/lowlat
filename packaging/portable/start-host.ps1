# Start the host from this folder with nothing installed: log in with a Parsec
# account when the folder holds no session yet, then run lowlatd as the system
# account in the console session, the shape the service gives it, in a window
# of its own on the desktop. Ctrl+C or closing that window stops the host.
#
#   powershell -ExecutionPolicy Bypass -File start-host.ps1 [lowlatd flags]
#   ... start-host.ps1 -Login     log in again first
#   ... start-host.ps1 -Logout    revoke the session the folder holds
#
# Run from an elevated shell: PsExec64 needs one to reach the system account.
# The login needs Python 3. The session is kept in lowlatd.env here, readable
# by the system account and administrators alone; the host reads every other
# variable written there too (KESSEL_WS_SERVER, LOWLAT_STUN). Its log is
# lowlatd.log here, appended to at every start.

param([switch]$Login, [switch]$Logout, [switch]$AsSystem)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$envFile = Join-Path $here 'lowlatd.env'
$log = Join-Path $here 'lowlatd.log'

if ($AsSystem) {
    $Host.UI.RawUI.WindowTitle = 'lowlat host'
    # The service's rules for the file: a line is KEY=VALUE, both trimmed, and
    # an empty line or one starting with # is skipped.
    foreach ($line in Get-Content -LiteralPath $envFile) {
        $line = $line.Trim()
        if (-not $line -or $line.StartsWith('#')) { continue }
        $key, $value = $line -split '=', 2
        if ($null -eq $value -or -not $key.Trim()) { continue }
        [Environment]::SetEnvironmentVariable($key.Trim(), $value.Trim(), 'Process')
    }
    # Shown here and appended to the log, in UTF-8. The host's lines arrive on
    # its error stream, so a stop on them is turned off.
    $ErrorActionPreference = 'Continue'
    $writer = New-Object System.IO.StreamWriter($log, $true)
    $writer.AutoFlush = $true
    try {
        & (Join-Path $here 'lowlatd.exe') @args 2>&1 | ForEach-Object {
            $text = "$_"
            $writer.WriteLine($text)
            $text
        }
    } finally {
        $writer.Dispose()
    }
    Write-Host "lowlatd ended, exit code $LASTEXITCODE; the log is $log"
    Read-Host 'press Enter to close'
    return
}

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'start-host.ps1 needs an elevated shell (Run as administrator)'
}

function Find-Python {
    $ErrorActionPreference = 'Continue'
    foreach ($try in @(@{ Name = 'py'; Pre = @('-3') }, @{ Name = 'python'; Pre = @() })) {
        $command = Get-Command $try.Name -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if (-not $command) { continue }
        $pre = $try.Pre
        # The Store's stand-in answers to the name and runs nothing.
        & $command.Source @pre -c 'import sys' *> $null
        if ($LASTEXITCODE -eq 0) { return @{ Exe = $command.Source; Pre = $pre } }
    }
    throw 'the login needs Python 3 (python.org, or: winget install Python.Python.3.12)'
}

function Invoke-Login([string[]]$Arguments) {
    $python = Find-Python
    $pre = $python.Pre
    & $python.Exe @pre (Join-Path $here 'lowlat-login.py') @Arguments --file $envFile
    if ($LASTEXITCODE -ne 0) { throw "the login ended with exit code $LASTEXITCODE" }
}

function Get-Session {
    if (-not (Test-Path -LiteralPath $envFile)) { return '' }
    $line = Get-Content -LiteralPath $envFile | Where-Object { $_ -match '^\s*KESSEL_SESSION=' } |
        Select-Object -First 1
    if ($line) { return ($line -split '=', 2)[1].Trim() }
    return ''
}

if ($Logout) {
    Invoke-Login @('--logout')
    return
}

$running = Get-Process -Name lowlatd -ErrorAction SilentlyContinue
if ($running) {
    throw "a host runs here already (pid $($running.Id -join ', ')); stop it, or the installed service (sc.exe stop lowlatd), first"
}

if ($Login -or -not (Get-Session)) {
    # Made before the login writes it, so the session never lands in a file
    # others can read; the login writes in place and keeps this access.
    if (-not (Test-Path -LiteralPath $envFile)) { New-Item -ItemType File -Path $envFile | Out-Null }
    & icacls.exe $envFile /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "$envFile keeps the folder's access (icacls exit $LASTEXITCODE)" }
    Invoke-Login @()
}

# -i alone is the console session; -d returns at once, with the started
# process's id as the exit code.
$shell = (Get-Process -Id $PID).Path
$inner = @('-accepteula', '-nobanner', '-s', '-i', '-d',
    $shell, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-AsSystem') + $args
& (Join-Path $here 'PsExec64.exe') @inner
$started = $LASTEXITCODE
if ($started -le 0 -or -not (Get-Process -Id $started -ErrorAction SilentlyContinue)) {
    throw "PsExec64 did not start the host (exit code $started)"
}
Write-Host "the host runs in its own window on the desktop (pid $started); its log is $log"
