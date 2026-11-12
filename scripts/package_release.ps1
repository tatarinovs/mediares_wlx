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
$succeeded = $false

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

function Publish-File([string]$Source, [string]$Name) {
    # Destination temp is unique; an error never deletes or truncates the previous artifact.
    $destination = [IO.Path]::Combine($distPath, $Name)
    $temporary = [IO.Path]::Combine($distPath, ('.mediares-' + [Guid]::NewGuid().ToString('N') + '.tmp'))
    try {
        [IO.File]::Copy($Source, $temporary, $false)
        if ([IO.File]::Exists($destination)) { [IO.File]::Replace($temporary, $destination, [NullString]::Value) }
        else { [IO.File]::Move($temporary, $destination) }
    } finally {
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
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
    Publish-File $wdxZip ([IO.Path]::GetFileName($wdxZip))
    Publish-File $wlxZip ([IO.Path]::GetFileName($wlxZip))
    Publish-File $combo 'mediares.wlx64'
    Publish-File $wdx 'mediares_wdx_only.wdx64'
    Publish-File (Resolve-Input 'pluginst/pluginst-wlx.inf') 'pluginst.inf'
    $succeeded = $true
    Write-Output "Verified release archives and DLLs written to $distPath"
} catch {
    Write-Error "Release packaging failed: $_"
    exit 1
} finally {
    # Check the resolved absolute target before the only recursive deletion.
    $resolvedStage = [IO.Path]::GetFullPath($stagePath)
    $allowedPrefix = [IO.Path]::GetFullPath($targetPath).TrimEnd('\') + '\'
    if ($succeeded -and $resolvedStage.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedStage)) {
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
