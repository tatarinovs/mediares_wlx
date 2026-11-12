$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$fixture = Join-Path $repo ('target/package-test-' + [Guid]::NewGuid().ToString('N'))
foreach ($dir in @('input', 'pluginst', 'docs')) { [IO.Directory]::CreateDirectory((Join-Path $fixture $dir)) | Out-Null }
foreach ($name in @('input/combo.dll', 'input/wdx.dll', 'pluginst/pluginst-wlx.inf', 'pluginst/pluginst-wdx.inf', 'pluginst/mediares.lng', 'pluginst/mediares_ui.lng', 'docs/readme_rus.txt', 'docs/readme_eng.txt')) {
    [IO.File]::WriteAllText((Join-Path $fixture $name), "packaging fixture: $name")
}
$package = Join-Path $PSScriptRoot 'package_release.ps1'
function Run-Packager {
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue' # A failing child is an expected test result.
    & powershell -NoProfile -ExecutionPolicy Bypass -File $package -Root $fixture -Version '0.0.0' -ComboDll 'input/combo.dll' -WdxDll 'input/wdx.dll' *> (Join-Path $fixture 'last-run.log')
    $result = $LASTEXITCODE
    $ErrorActionPreference = $previousPreference
    return $result
}
if ((Run-Packager) -ne 0) { throw 'Valid fixture did not package successfully' }
$archive = Join-Path $fixture 'dist/mediares-wdx-v0.0.0.zip'
$before = (Get-FileHash -LiteralPath $archive).Hash
$translation = Join-Path $fixture 'pluginst/mediares_ui.lng'
[IO.File]::Delete($translation)
if ((Run-Packager) -eq 0) { throw 'Missing required source was accepted' }
if ((Get-FileHash -LiteralPath $archive).Hash -ne $before) { throw 'Failed staging changed the published archive' }
[IO.File]::WriteAllText($translation, 'restored translation')
$locked = [IO.File]::Open($archive, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    if ((Run-Packager) -eq 0) { throw 'Locked destination was reported as success' }
    if ((Get-FileHash -LiteralPath $archive).Hash -ne $before) { throw 'Failed publication damaged the previous archive' }
} finally { $locked.Dispose() }
if ((Run-Packager) -ne 0) { throw 'Retry after releasing destination failed' }
Write-Output "Packaging tests passed: verified archives, missing source, locked destination and retry ($fixture)"
