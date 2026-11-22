[CmdletBinding()]
param(
    [uint32]$Display = 0,
    [ValidateRange(1,64)][int]$Threads = [Math]::Min(6,[Math]::Max(1,[Environment]::ProcessorCount / 2)),
    [ValidateRange(1,300)][int]$Seconds = 30,
    [switch]$CpuOnly
)
$ErrorActionPreference = 'Stop'
$recorder = Join-Path $PSScriptRoot 'target\release\instantreplayrust.exe'
if (!(Test-Path -LiteralPath $recorder)) {
    throw 'Build the recorder first with cargo build --release --locked.'
}
$clips = Join-Path $PSScriptRoot 'clips'
[void][IO.Directory]::CreateDirectory($clips)
$log = Join-Path $clips ('replay-' + [guid]::NewGuid().ToString('N') + '.log')
$recorderArgs = @('prores-replay','--threads',"$Threads",'--display',"$Display",'--seconds',"$Seconds")
if ($CpuOnly) { $recorderArgs += '--cpu-only' }
$process = Start-Process -FilePath $recorder -ArgumentList $recorderArgs -WorkingDirectory $PSScriptRoot -WindowStyle Hidden -RedirectStandardError $log -PassThru
Start-Sleep -Seconds 2
if ($process.HasExited) {
    Get-Content -LiteralPath $log
    throw 'Replay failed to start.'
}
Write-Host "Replay running in background (PID $($process.Id)). F5 saves the last $Seconds seconds. Ctrl+Alt+F10 stops."
Write-Host "Status and saved clip paths are in $log"
