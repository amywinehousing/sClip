[CmdletBinding()]
param([string]$BinDirectory = (Join-Path $PSScriptRoot '..\target\release'))

$ErrorActionPreference = 'Stop'
# 64-bit Windows build listed by mpv.io. Verify the release asset before extraction.
$asset = 'mpv-dev-x86_64-20261111-git-bcde6b93cc.7z'
$url = "https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20261111/$asset"
$hash = '273c079b11ea5575822f972756f4c9a71149a5c10130353abeb8586738938452'
$cache = Join-Path $PSScriptRoot '..\.tools\clip-viewer'
$player = Join-Path $BinDirectory 'player'
[void][IO.Directory]::CreateDirectory($cache)
$archive = Join-Path $cache $asset
if (!(Test-Path -LiteralPath $archive) -or (Get-FileHash -LiteralPath $archive).Hash -ne $hash) {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive).Hash -ne $hash) { throw 'Playback archive checksum mismatch.' }
$zip = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source
if (!$zip) { $zip = Join-Path $env:ProgramFiles '7-Zip\7z.exe' }
if (!(Test-Path -LiteralPath $zip)) { throw 'Install 7-Zip to extract the playback runtime.' }
& $zip x -y "-o$cache" $archive libmpv-2.dll
if ($LASTEXITCODE) { throw 'Playback runtime extraction failed.' }
foreach ($name in 'Copyright','LICENSE.GPL','LICENSE.LGPL') {
    Invoke-WebRequest -UseBasicParsing -Uri "https://raw.githubusercontent.com/mpv-player/mpv/bcde6b93cc/$name" -OutFile (Join-Path $cache $name)
}
[void][IO.Directory]::CreateDirectory($player)
foreach ($name in 'libmpv-2.dll','Copyright','LICENSE.GPL','LICENSE.LGPL') {
    Copy-Item -LiteralPath (Join-Path $cache $name) -Destination $player
}
Write-Host "Installed clip viewer runtime in $player."
