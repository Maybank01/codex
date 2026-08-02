[CmdletBinding()]
param(
    [string[]]$CompatibleShellVersions,
    [string]$SourcePackageVersion,
    [string]$OutputDirectory,
    [ValidateSet("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$BinaryPath,
    [switch]$SkipBuild,
    [switch]$AllowDirty
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repoRoot = Split-Path -Parent $PSScriptRoot
$cargoRoot = Join-Path $repoRoot "codex-rs"
$releaseConfigPath = Join-Path $PSScriptRoot "agentrouter-core-release.json"
$releaseConfig = Get-Content -LiteralPath $releaseConfigPath -Raw | ConvertFrom-Json
$releaseBranch = "agentrouter/runtime-components-v2"
$releaseRemote = "fork"
$releaseRemoteUrl = "https://github.com/Maybank01/codex.git"
$releaseRemoteRef = "refs/heads/$releaseBranch"

if ($null -eq $CompatibleShellVersions -or $CompatibleShellVersions.Count -eq 0) {
    $CompatibleShellVersions = @($releaseConfig.compatible_shell_versions)
}
if ([string]::IsNullOrWhiteSpace($SourcePackageVersion)) {
    $SourcePackageVersion = [string]$releaseConfig.source_package_version
}
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repoRoot "dist\agentrouter-core"
}

foreach ($shellVersion in $CompatibleShellVersions) {
    if ($shellVersion -cnotmatch '^\d+\.\d+\.\d+$') {
        throw "Invalid compatible shell version: $shellVersion."
    }
}
if ($SourcePackageVersion -cnotmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Invalid MSIX source package version: $SourcePackageVersion."
}

$cargoManifest = Get-Content -LiteralPath (Join-Path $cargoRoot "Cargo.toml") -Raw
$versionMatch = [regex]::Match(
    $cargoManifest,
    '(?m)^version\s*=\s*"([^"]+)"\s*$'
)
if (-not $versionMatch.Success) {
    throw "Could not read the workspace version from codex-rs/Cargo.toml."
}
$version = $versionMatch.Groups[1].Value
if ($version -cne [string]$releaseConfig.version) {
    throw "Cargo version $version does not match AgentRouter release config version $($releaseConfig.version)."
}
$upstreamVersion = [string]$releaseConfig.upstream_version
$agentRouterVersion = [string]$releaseConfig.agentrouter_version
$packageRevision = [int]$releaseConfig.package_revision
$releaseSequence = [int]$releaseConfig.release_sequence
$displayVersion = [string]$releaseConfig.display_version
if ($upstreamVersion -cnotmatch '^\d+\.\d+\.\d+$') {
    throw "Invalid upstream Core version: $upstreamVersion."
}
if ($agentRouterVersion -cnotmatch '^\d+\.\d+\.\d+$') {
    throw "Invalid AgentRouter Core version: $agentRouterVersion."
}
if ($packageRevision -lt 1 -or $releaseSequence -lt 1) {
    throw "AgentRouter package revision and release sequence must be positive."
}
$expectedInternalVersion = "$upstreamVersion-agentrouter.$agentRouterVersion"
if ($version -cne $expectedInternalVersion) {
    throw "Core version $version does not match upstream/custom versions ($expectedInternalVersion)."
}
$expectedDisplayVersion = "u$upstreamVersion-ar$agentRouterVersion-r$packageRevision"
if ($displayVersion -cne $expectedDisplayVersion) {
    throw "Display version $displayVersion does not match $expectedDisplayVersion."
}

$sourceSha = (& git -C $repoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not resolve the source Git SHA."
}
$sourceBranch = (& git -C $repoRoot branch --show-current).Trim()
$gitStatus = (& git -C $repoRoot status --porcelain --untracked-files=all) -join "`n"
if (-not [string]::IsNullOrWhiteSpace($gitStatus) -and -not $AllowDirty) {
    throw "Refusing to build a release artifact from a dirty worktree. Pass -AllowDirty for a development artifact."
}
$sourceDirty = -not [string]::IsNullOrWhiteSpace($gitStatus)
$sourceRemoteSha = $null
$productionEligible = $false
if (-not $AllowDirty) {
    if (
        -not [string]::IsNullOrWhiteSpace($sourceBranch) -and
        $sourceBranch -cne $releaseBranch
    ) {
        throw "Release Core must be built from branch $releaseBranch; got $sourceBranch. Pass -AllowDirty only for development evidence."
    }
    $actualRemoteUrl = (& git -C $repoRoot remote get-url $releaseRemote).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Could not resolve release remote $releaseRemote."
    }
    $normalizeRemoteUrl = {
        param([string]$Value)
        $normalized = $Value.Trim().Replace("\", "/")
        if ($normalized.StartsWith("git@github.com:")) {
            $normalized = "https://github.com/" + $normalized.Substring("git@github.com:".Length)
        }
        $normalized = $normalized.TrimEnd("/")
        if ($normalized.EndsWith(".git")) {
            $normalized = $normalized.Substring(0, $normalized.Length - 4)
        }
        return $normalized.ToLowerInvariant()
    }
    if ((& $normalizeRemoteUrl $actualRemoteUrl) -cne (& $normalizeRemoteUrl $releaseRemoteUrl)) {
        throw "Release remote $releaseRemote must be $releaseRemoteUrl; got $actualRemoteUrl."
    }
    $remoteLines = @(& git -C $repoRoot ls-remote --heads $releaseRemote $releaseRemoteRef)
    if ($LASTEXITCODE -ne 0 -or $remoteLines.Count -ne 1) {
        throw "Release remote ref $releaseRemote/$releaseRemoteRef did not resolve exactly once."
    }
    $remoteMatch = [regex]::Match(
        [string]$remoteLines[0],
        '^([0-9a-f]{40})\s+refs/heads/agentrouter/runtime-components-v2$'
    )
    if (-not $remoteMatch.Success) {
        throw "Release remote ref $releaseRemote/$releaseRemoteRef returned an invalid commit."
    }
    $sourceRemoteSha = $remoteMatch.Groups[1].Value
    if ($sourceSha -cne $sourceRemoteSha) {
        throw "Release Core source HEAD $sourceSha does not match pushed fixed branch $sourceRemoteSha."
    }
    $productionEligible = $true
}

$upstreamSha = [string]$releaseConfig.upstream_sha
& git -C $repoRoot merge-base --is-ancestor $upstreamSha HEAD
if ($LASTEXITCODE -ne 0) {
    throw "Configured upstream SHA $upstreamSha is not an ancestor of HEAD."
}

switch ($Target) {
    "x86_64-pc-windows-msvc" {
        $architecture = "x64"
        $artifactPlatform = "win-x64"
    }
    "aarch64-pc-windows-msvc" {
        $architecture = "arm64"
        $artifactPlatform = "win-arm64"
    }
}

if (-not $SkipBuild) {
    Push-Location $cargoRoot
    try {
        if ($Target -eq "x86_64-pc-windows-msvc") {
            $env:LIBSQLITE3_FLAGS = "SQLITE_DISABLE_INTRINSIC"
        }
        & cargo build --locked --target $Target --release --bin codex |
            ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo failed to build the AgentRouter Codex Core binary."
        }
    } finally {
        Pop-Location
    }
}

if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $cargoRoot "target\$Target\release\codex.exe"
}
$resolvedBinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path
$binaryStream = [System.IO.File]::OpenRead($resolvedBinaryPath)
$binaryReader = [System.IO.BinaryReader]::new($binaryStream)
try {
    if ($binaryStream.Length -lt 64 -or $binaryReader.ReadUInt16() -ne 0x5A4D) {
        throw "Core binary is not a valid PE executable."
    }
    $binaryStream.Position = 0x3C
    $peOffset = $binaryReader.ReadUInt32()
    if ($peOffset -gt ($binaryStream.Length - 6)) {
        throw "Core binary has an invalid PE header offset."
    }
    $binaryStream.Position = $peOffset
    if ($binaryReader.ReadUInt32() -ne 0x00004550) {
        throw "Core binary is missing the PE signature."
    }
    $actualMachine = $binaryReader.ReadUInt16()
} finally {
    $binaryReader.Dispose()
}
$expectedMachine = switch ($Target) {
    "x86_64-pc-windows-msvc" { 0x8664 }
    "aarch64-pc-windows-msvc" { 0xAA64 }
}
if ($actualMachine -ne $expectedMachine) {
    throw (
        "Core binary PE machine 0x{0:X4} does not match target {1} (expected 0x{2:X4})." -f
            $actualMachine,
            $Target,
            $expectedMachine
    )
}
$versionOutput = (& $resolvedBinaryPath --version).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -ne "codex-cli $version") {
    throw "Unexpected Codex binary version '$versionOutput'; expected 'codex-cli $version'."
}

$binaryItem = Get-Item -LiteralPath $resolvedBinaryPath
$binarySha256 = (Get-FileHash -LiteralPath $resolvedBinaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
$signatureStatus = [string](Get-AuthenticodeSignature -LiteralPath $resolvedBinaryPath).Status

$resolvedOutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
[System.IO.Directory]::CreateDirectory($resolvedOutputDirectory) | Out-Null
$artifactName = "CodexCore-$artifactPlatform-$displayVersion"
$archivePath = Join-Path $resolvedOutputDirectory "$artifactName.zip"
$checksumPath = Join-Path $resolvedOutputDirectory "$artifactName.zip.sha256"
$stagingRoot = Join-Path (
    [System.IO.Path]::GetTempPath()
) "agentrouter-core-$([Guid]::NewGuid().ToString('N'))"
$resourcesDirectory = Join-Path $stagingRoot "resources"
$manifestPath = Join-Path $stagingRoot "agentrouter-core.json"

try {
    [System.IO.Directory]::CreateDirectory($resourcesDirectory) | Out-Null
    Copy-Item -LiteralPath $resolvedBinaryPath -Destination (Join-Path $resourcesDirectory "codex.exe")

    $manifest = [ordered]@{
        schemaVersion = 1
        kind = "agentrouter-codex-core"
        version = $version
        displayVersion = $displayVersion
        upstreamVersion = $upstreamVersion
        agentrouterVersion = $agentRouterVersion
        packageRevision = $packageRevision
        releaseSequence = $releaseSequence
        entrypoint = "resources/codex.exe"
        compatibleShellVersions = @($CompatibleShellVersions)
        upstreamGitSha = $upstreamSha
        sourcePackageVersion = $SourcePackageVersion
        platform = "windows"
        arch = $architecture
        rustTarget = $Target
        upstreamRepository = [string]$releaseConfig.upstream_repository
        sourceGitSha = $sourceSha
        sourceBranch = $(if ([string]::IsNullOrWhiteSpace($sourceBranch)) { "<detached>" } else { $sourceBranch })
        releaseBranch = $releaseBranch
        sourceRemote = $releaseRemote
        sourceRemoteSha = $sourceRemoteSha
        sourceDirty = $sourceDirty
        productionEligible = $productionEligible
        signatureStatus = $signatureStatus
        files = @(
            [ordered]@{
                path = "resources/codex.exe"
                size = $binaryItem.Length
                sha256 = $binarySha256
            }
        )
        generatedAtUtc = [DateTime]::UtcNow.ToString("o")
    }
    [System.IO.File]::WriteAllText(
        $manifestPath,
        (($manifest | ConvertTo-Json -Depth 8) + "`n"),
        [System.Text.UTF8Encoding]::new($false)
    )

    if (Test-Path -LiteralPath $archivePath) {
        Remove-Item -LiteralPath $archivePath -Force
    }
    Compress-Archive -LiteralPath @($resourcesDirectory, $manifestPath) -DestinationPath $archivePath

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $entries = @($archive.Entries | ForEach-Object { $_.FullName })
    } finally {
        $archive.Dispose()
    }
    $expectedEntries = @("agentrouter-core.json", "resources/codex.exe")
    $entryDifference = @(
        Compare-Object -ReferenceObject $expectedEntries -DifferenceObject $entries
    )
    if ($entryDifference.Count -ne 0) {
        throw "Core archive contents were not exactly agentrouter-core.json and resources/codex.exe."
    }

    $archiveSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText(
        $checksumPath,
        "$archiveSha256  $([System.IO.Path]::GetFileName($archivePath))`n",
        [System.Text.UTF8Encoding]::new($false)
    )
} finally {
    if (Test-Path -LiteralPath $stagingRoot -PathType Container) {
        $resolvedStagingRoot = [System.IO.Path]::GetFullPath($stagingRoot)
        $resolvedTempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
        if (-not $resolvedStagingRoot.StartsWith(
            $resolvedTempRoot,
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Refusing to remove staging directory outside the system temp directory."
        }
        Remove-Item -LiteralPath $resolvedStagingRoot -Recurse -Force
    }
}

[PSCustomObject]@{
    Version = $version
    DisplayVersion = $displayVersion
    ArchivePath = $archivePath
    ChecksumPath = $checksumPath
    BinarySha256 = $binarySha256
    SourceSha = $sourceSha
    SourceBranch = $sourceBranch
    SourceRemoteSha = $sourceRemoteSha
    SourceDirty = $sourceDirty
    ProductionEligible = $productionEligible
    CompatibleShellVersions = @($CompatibleShellVersions)
    SourcePackageVersion = $SourcePackageVersion
}
