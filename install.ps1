[CmdletBinding()]
param(
    [string]$InstallDir = $env:CLAUDEX_INSTALL_DIR,
    [string]$Version = "latest",
    [switch]$NoPathUpdate,
    [string]$Repository = "josephismikhail/claudex"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    throw "install.ps1 supports Windows only. Use install.sh on Linux or macOS."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\Claudex\bin"
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$target = switch ($architecture) {
    "X64" { "x86_64-pc-windows-msvc" }
    default { throw "Unsupported Windows architecture: $architecture (currently supported: x64)" }
}

$headers = @{
    "Accept" = "application/vnd.github+json"
    "User-Agent" = "claudex-installer"
}

if ($Version -eq "latest") {
    $release = Invoke-RestMethod `
        -Uri "https://api.github.com/repos/$Repository/releases/latest" `
        -Headers $headers
    $tag = [string]$release.tag_name
} elseif ($Version.StartsWith("v")) {
    $tag = $Version
} else {
    $tag = "v$Version"
}

if ([string]::IsNullOrWhiteSpace($tag)) {
    throw "GitHub did not return a release tag for $Repository."
}

$assetName = "claudex-$tag-$target.zip"
$downloadUrl = "https://github.com/$Repository/releases/download/$tag/$assetName"
$checksumUrl = "$downloadUrl.sha256"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) "claudex-install-$([guid]::NewGuid().ToString('N'))"

Write-Host "Claudex Windows installer"
Write-Host "  Repository:   $Repository"
Write-Host "  Release:      $tag"
Write-Host "  Architecture: $target"
Write-Host "  Install path: $InstallDir"

New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
$stagedDestination = $null
try {
    $archivePath = Join-Path $tempDir $assetName
    $checksumPath = "$archivePath.sha256"

    Invoke-WebRequest -Uri $downloadUrl -Headers $headers -OutFile $archivePath
    Invoke-WebRequest -Uri $checksumUrl -Headers $headers -OutFile $checksumPath

    $expectedHash = ((Get-Content -Raw -LiteralPath $checksumPath).Trim() -split "\s+")[0]
    $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash
    if (-not $actualHash.Equals($expectedHash, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "SHA-256 verification failed for $assetName."
    }

    $extractDir = Join-Path $tempDir "extracted"
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force
    $sourceBinary = Get-ChildItem -LiteralPath $extractDir -Filter "claudex.exe" -File -Recurse |
        Select-Object -First 1
    if ($null -eq $sourceBinary) {
        throw "The release archive does not contain claudex.exe."
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destination = Join-Path $InstallDir "claudex.exe"
    $stagedDestination = Join-Path $InstallDir "claudex.exe.new"
    Copy-Item -LiteralPath $sourceBinary.FullName -Destination $stagedDestination -Force
    Unblock-File -LiteralPath $stagedDestination -ErrorAction SilentlyContinue
    Move-Item -LiteralPath $stagedDestination -Destination $destination -Force

    if (-not $NoPathUpdate) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $pathEntries = @($userPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $alreadyPresent = $pathEntries | Where-Object {
            $_.TrimEnd("\") -eq $InstallDir.TrimEnd("\")
        }
        if (-not $alreadyPresent) {
            $updatedPath = (@($pathEntries) + $InstallDir) -join ";"
            [Environment]::SetEnvironmentVariable("Path", $updatedPath, "User")
            Write-Host "  Added the install directory to your user PATH."
        }
    }

    if (-not (($env:Path -split ";") -contains $InstallDir)) {
        $env:Path = "$InstallDir;$env:Path"
    }

    Write-Host ""
    Write-Host "Installed and verified:"
    & $destination --version

    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
        Write-Warning "Claude Code was not found in PATH. Install it before running 'claudex run'."
    }
} finally {
    if ($null -ne $stagedDestination) {
        Remove-Item -LiteralPath $stagedDestination -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
