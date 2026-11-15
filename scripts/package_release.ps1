param(
    [Parameter(Mandatory = $true)][string]$Root,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$')][string]$Version,
    [Parameter(Mandatory = $true)][string]$ComboDll,
    [Parameter(Mandatory = $true)][string]$WdxDll
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$rootPath = [IO.Path]::GetFullPath($Root)
$targetPath = [IO.Path]::Combine($rootPath, 'target')
$stagePath = [IO.Path]::Combine($targetPath, ('package-' + [Guid]::NewGuid().ToString('N')))
$distPath = [IO.Path]::Combine($rootPath, 'dist')
$keepBackups = $false

function Resolve-Input([string]$Name) {
    $path = if ([IO.Path]::IsPathRooted($Name)) { $Name } else { [IO.Path]::Combine($rootPath, $Name) }
    $item = Get-Item -LiteralPath $path -ErrorAction Stop
    if ($item.PSIsContainer) { throw "Expected a file: $path" }
    return $item.FullName
}

function Stage-Package([string]$Kind, [string]$Binary) {
    $dir = [IO.Path]::Combine($stagePath, $Kind)
    [IO.Directory]::CreateDirectory($dir) | Out-Null
    $extension = if ($Kind -eq 'wlx') { 'wlx64' } else { 'wdx64' }
    $files = [ordered]@{
        "mediares.$extension" = $Binary
        'pluginst.inf' = (Resolve-Input "pluginst/pluginst-$Kind.inf")
        'mediares.lng' = (Resolve-Input 'pluginst/mediares.lng')
        'readme_rus.txt' = (Resolve-Input 'docs/readme_rus.txt')
        'readme_eng.txt' = (Resolve-Input 'docs/readme_eng.txt')
    }
    if ($Kind -eq 'wlx') { $files['mediares_ui.lng'] = Resolve-Input 'pluginst/mediares_ui.lng' }
    foreach ($entry in $files.GetEnumerator()) {
        Copy-Item -LiteralPath $entry.Value -Destination ([IO.Path]::Combine($dir, $entry.Key))
    }
    $zipPath = [IO.Path]::Combine($stagePath, "mediares-$Kind-v$Version.zip")
    [IO.Compression.ZipFile]::CreateFromDirectory($dir, $zipPath)
    $zip = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        if ($zip.Entries.Count -ne $files.Count) { throw "Unexpected archive contents: $zipPath" }
        foreach ($entry in $files.GetEnumerator()) {
            $packed = $zip.GetEntry($entry.Key)
            if ($null -eq $packed) { throw "Missing $($entry.Key) in $zipPath" }
            $stream = $packed.Open()
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $packedHash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
            finally { $stream.Dispose(); $sha.Dispose() }
            if ($packedHash -ne (Get-FileHash -LiteralPath $entry.Value -Algorithm SHA256).Hash) {
                throw "Archive checksum mismatch: $($entry.Key)"
            }
        }
    } finally { $zip.Dispose() }
    return $zipPath
}

function Publish-Release($Files) {
    $prepared = @()
    $changed = @()
    $leases = @()
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $key = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::Unicode.GetBytes($distPath.ToUpperInvariant()))).Replace('-', '') }
    finally { $sha.Dispose() }
    $mutex = New-Object Threading.Mutex($false, ('Local\MediaresPackage-' + $key))
    $held = $false
    try {
        try { $held = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $held = $true }
        if (-not $held) { throw 'Another release is being published to this directory' }
        # Stage every input and test every existing destination before changing any artifact.
        foreach ($entry in $Files.GetEnumerator()) {
            $destination = [IO.Path]::Combine($distPath, $entry.Key)
            $temporary = [IO.Path]::Combine($distPath, ('.mediares-' + [Guid]::NewGuid().ToString('N') + '.tmp'))
            $backup = [IO.Path]::Combine($stagePath, [Guid]::NewGuid().ToString('N') + '.bak')
            $existed = [IO.File]::Exists($destination)
            $item = [pscustomobject]@{ Destination=$destination; Temporary=$temporary; Backup=$backup; Existed=$existed }
            $prepared += $item
            if ($existed) {
                $leases += [IO.File]::Open($destination, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, ([IO.FileShare]::Read -bor [IO.FileShare]::Delete))
                [IO.File]::Copy($destination, $backup, $false)
            }
            [IO.File]::Copy($entry.Value, $temporary, $false)
        }
        foreach ($item in $prepared) {
            if ($item.Existed) { [IO.File]::Replace($item.Temporary, $item.Destination, [NullString]::Value) }
            else { [IO.File]::Move($item.Temporary, $item.Destination) }
            $changed += $item
        }
    } catch {
        $failure = $_
        $rollbackErrors = @()
        [array]::Reverse($changed)
        foreach ($item in $changed) {
            try {
                if ($item.Existed) { [IO.File]::Replace($item.Backup, $item.Destination, [NullString]::Value) }
                else { [IO.File]::Delete($item.Destination) }
            } catch { $rollbackErrors += $_.ToString() }
        }
        if ($rollbackErrors.Count) {
            $script:keepBackups = $true
            throw "Publication failed: $failure. Rollback errors: $($rollbackErrors -join '; '). Backups: $stagePath"
        }
        throw $failure
    } finally {
        foreach ($lease in $leases) { $lease.Dispose() }
        foreach ($item in $prepared) {
            if ([IO.File]::Exists($item.Temporary)) { [IO.File]::Delete($item.Temporary) }
        }
        if ($held) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}

try {
    $combo = Resolve-Input $ComboDll
    $wdx = Resolve-Input $WdxDll
    [IO.Directory]::CreateDirectory($stagePath) | Out-Null
    # Complete and verify both archives before publishing any artifact.
    $wdxZip = Stage-Package 'wdx' $wdx
    $wlxZip = Stage-Package 'wlx' $combo
    [IO.Directory]::CreateDirectory($distPath) | Out-Null
    Publish-Release ([ordered]@{
        ([IO.Path]::GetFileName($wdxZip)) = $wdxZip
        ([IO.Path]::GetFileName($wlxZip)) = $wlxZip
        'mediares.wlx64' = $combo
        'mediares_wdx_only.wdx64' = $wdx
        'mediares.lng' = (Resolve-Input 'pluginst/mediares.lng')
        'mediares_ui.lng' = (Resolve-Input 'pluginst/mediares_ui.lng')
        'pluginst.inf' = (Resolve-Input 'pluginst/pluginst-wlx.inf')
    })
    Write-Output "Verified release archives and DLLs written to $distPath"
} catch {
    Write-Error "Release packaging failed: $_"
    exit 1
} finally {
    # Check the resolved absolute target before the only recursive deletion.
    $resolvedStage = [IO.Path]::GetFullPath($stagePath)
    $allowedPrefix = [IO.Path]::GetFullPath($targetPath).TrimEnd('\') + '\'
    if (-not $keepBackups -and $resolvedStage.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedStage)) {
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
