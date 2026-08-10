# Download and install a published GCABB release on Windows.
param(
    [string]$Tag
)

$ErrorActionPreference = "Stop"
$OriginalProgressPreference = $ProgressPreference
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$Repository = if ($env:GCABB_REPOSITORY) {
    $env:GCABB_REPOSITORY
} else {
    "constructomech/gcabb"
}
$InstallDir = if ($env:GCABB_INSTALL_DIR) {
    [Environment]::ExpandEnvironmentVariables($env:GCABB_INSTALL_DIR)
} else {
    Join-Path $env:LOCALAPPDATA "GCABB"
}

if ($env:OS -ne "Windows_NT") {
    throw "This installer supports Windows only."
}
$Architecture = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}
if ($Architecture -notin @("AMD64", "ARM64")) {
    throw "No published GCABB build supports Windows $Architecture."
}

if (-not $Tag) {
    $headers = @{
        Accept = "application/vnd.github+json"
        "User-Agent" = "gcabb-installer"
    }
    $releases = Invoke-RestMethod `
        -Uri "https://api.github.com/repos/$Repository/releases?per_page=1" `
        -Headers $headers
    $Tag = @($releases)[0].tag_name
}
if (-not $Tag) {
    throw "$Repository has no published releases."
}
if (-not $Tag.StartsWith("v")) {
    throw "Release tag must start with v: $Tag"
}

$Version = $Tag.Substring(1)
$Target = "x86_64-pc-windows-msvc"
$Asset = "gcabb-$Version-$Target.zip"
$DownloadUrl = "https://github.com/$Repository/releases/download/$Tag/$Asset"
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$Parent = Split-Path -Parent $InstallDir
$Name = Split-Path -Leaf $InstallDir
$Staging = Join-Path $Parent ".$Name-installing"
$Backup = Join-Path $Parent ".$Name-previous"
$Work = Join-Path ([IO.Path]::GetTempPath()) "gcabb-install-$([guid]::NewGuid())"
$Archive = Join-Path $Work $Asset

New-Item -ItemType Directory -Force -Path $Parent, $Work | Out-Null
if ((Test-Path -LiteralPath $Staging) -or (Test-Path -LiteralPath $Backup)) {
    throw "A previous install did not finish. Inspect $Staging and $Backup."
}

try {
    Write-Host "Downloading GCABB $Version for $Target..."
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        # Windows PowerShell 5.1 redraws progress for nearly every received byte,
        # making large downloads dramatically slower.
        $ProgressPreference = "SilentlyContinue"
    }
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $Archive -UseBasicParsing
    $ProgressPreference = $OriginalProgressPreference
    Expand-Archive -LiteralPath $Archive -DestinationPath $Staging

    $Executable = Join-Path $Staging "gcabb-desktop.exe"
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        throw "$Asset does not contain gcabb-desktop.exe."
    }

    $HadInstall = Test-Path -LiteralPath $InstallDir
    if ($HadInstall) {
        try {
            Move-Item -LiteralPath $InstallDir -Destination $Backup
        } catch {
            throw "Could not replace $InstallDir. Close GCABB and run the installer again. $($_.Exception.Message)"
        }
    }
    try {
        Move-Item -LiteralPath $Staging -Destination $InstallDir
    } catch {
        if ($HadInstall -and (Test-Path -LiteralPath $Backup)) {
            Move-Item -LiteralPath $Backup -Destination $InstallDir
        }
        throw
    }
    if ($HadInstall) {
        Remove-Item -LiteralPath $Backup -Recurse -Force
    }

    Write-Host "Installed GCABB $Version at $InstallDir"
    Write-Host "Run:"
    Write-Host "  & `"$InstallDir\gcabb-desktop.exe`""
} finally {
    $ProgressPreference = $OriginalProgressPreference
    if (Test-Path -LiteralPath $Staging) {
        Remove-Item -LiteralPath $Staging -Recurse -Force
    }
    if (Test-Path -LiteralPath $Work) {
        Remove-Item -LiteralPath $Work -Recurse -Force
    }
}
