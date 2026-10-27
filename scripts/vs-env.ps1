#!/usr/bin/env pwsh
# Visual Studio's x64 developer environment in the calling session, when it has no `cl` already:
# the variables vcvars64 sets, taken from a child cmd. Dot-source it:
#
#   . scripts/vs-env.ps1
#
# The demo's toolkit is built once by its own nmake, with `cl` and the Windows SDK's `fxc`, and
# describe is compiled with `cl`; clang-cl, the demo's own compiler, needs none of this.

if (-not (Get-Command cl -ErrorAction SilentlyContinue)) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $vs) { throw "vs-env: no Visual Studio with the x64 tools" }
    $vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
    cmd /c "`"$vcvars`" >nul && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { Set-Item "Env:\$($Matches[1])" $Matches[2] }
    }
    if (-not (Get-Command cl -ErrorAction SilentlyContinue)) { throw "vs-env: vcvars64 left no cl" }
}
