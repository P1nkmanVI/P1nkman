# Runs the normal game profile while recording process/system/GPU memory.
# Stops the runtime before physical RAM falls below the configured safety line.
param(
    [Parameter(Mandatory)][string]$Exe,
    [ValidateSet('diag', 'release')][string]$Build = 'release',
    [ValidateSet('series-s', 'series-x')][string]$Console = 'series-s',
    [ValidateRange(1, 60)][int]$MaxMinutes = 15,
    [ValidateRange(2, 8)][int]$MinFreeGiB = 5,
    [switch]$DisableMemoryAutoStop,
    [string[]]$ShaderHashes = @()
)

$ErrorActionPreference = 'Stop'
$logDir = Join-Path $env:LOCALAPPDATA 'P1nkman-Coolfox\logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$csv = Join-Path $logDir "session-$stamp.csv"
$gpuCsv = Join-Path $logDir "session-gpu-$stamp.csv"
$gpuErrorCsv = Join-Path $logDir "session-gpu-errors-$stamp.csv"
$metadataJson = Join-Path $logDir "session-meta-$stamp.json"
$consoleOut = Join-Path $logDir "session-console-$stamp.txt"
$consoleErr = Join-Path $logDir "session-errors-$stamp.txt"
$shaderCaptureDir = $null
$started = Get-Date
$sessionClock = [Diagnostics.Stopwatch]::StartNew()
$cutoff = [uint64]$MinFreeGiB * 1GB
$ramHistory = [System.Collections.Generic.Queue[object]]::new()
$logicalProcessors = [Environment]::ProcessorCount
try {
    $reportedCores = [int](Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
    if ($reportedCores -gt 0) { $logicalProcessors = $reportedCores }
} catch { }

function Get-FreePhysicalBytes {
    $status = [VictoryMemoryStatus]::new()
    $status.dwLength = [Runtime.InteropServices.Marshal]::SizeOf($status)
    if (-not [VictoryMemoryNative]::GlobalMemoryStatusEx([ref]$status)) {
        throw "GlobalMemoryStatusEx falhou: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }
    return [pscustomobject]@{
        AvailablePhysicalBytes = [uint64]$status.ullAvailPhys
        CommitBytes = [uint64]($status.ullTotalPageFile - $status.ullAvailPageFile)
        CommitLimitBytes = [uint64]$status.ullTotalPageFile
    }
}

if (-not ('VictoryMemoryNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct VictoryMemoryStatus {
    public uint dwLength, dwMemoryLoad;
    public ulong ullTotalPhys, ullAvailPhys, ullTotalPageFile, ullAvailPageFile;
    public ulong ullTotalVirtual, ullAvailVirtual, ullAvailExtendedVirtual;
}
public static class VictoryMemoryNative {
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern bool GlobalMemoryStatusEx(ref VictoryMemoryStatus status);
}
'@
}

if (-not (Test-Path -LiteralPath $Exe)) { throw "Jogo não encontrado: $Exe" }
if ($ShaderHashes.Count -gt 16) { throw 'No máximo 16 hashes de shader por captura.' }
foreach ($hash in $ShaderHashes) {
    if ($hash -notmatch '^[0-9a-fA-F]{16}$') { throw "Hash de shader inválido: $hash" }
}
if (Get-Process victory, P1nkman -ErrorAction SilentlyContinue) {
    throw 'Já existe um processo do runtime. Feche-o antes de iniciar uma captura.'
}
if ((Get-FreePhysicalBytes).AvailablePhysicalBytes -le $cutoff) {
    throw "RAM livre está abaixo da margem de segurança de $MinFreeGiB GiB; captura não iniciada."
}

$projectExe = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if (-not (Test-Path -LiteralPath $projectExe)) { $projectExe = Join-Path $PSScriptRoot "..\target\$Build\P1nkman.exe" }
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$configPath = if ($env:VICTORY_CONFIG) { $env:VICTORY_CONFIG } else { Join-Path $repoRoot 'victory.toml' }
$projectExeHash = if (Test-Path -LiteralPath $projectExe) { (Get-FileHash -LiteralPath $projectExe -Algorithm SHA256).Hash } else { $null }
$cpuName = $null; $gpuInfo = @(); $pageFiles = @(); $gitCommit = $null; $gitBranch = $null
try { $cpuName = (Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name) } catch { }
try { $gpuInfo = @(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion) } catch { }
try { $pageFiles = @(Get-CimInstance Win32_PageFileUsage | Select-Object Name,AllocatedBaseSize,CurrentUsage) } catch { }
try {
    $repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    $gitCommit = (& git -C $repo rev-parse HEAD 2>$null).Trim()
    $gitBranch = (& git -C $repo branch --show-current 2>$null).Trim()
} catch { }
$metadata = [ordered]@{
    started = $started.ToString('o'); computer = $env:COMPUTERNAME; windows = [Environment]::OSVersion.VersionString
    git_commit = $gitCommit; git_branch = $gitBranch; cpu = $cpuName; gpus = $gpuInfo; page_files = $pageFiles; logical_processors = $logicalProcessors
    total_ram_bytes = [uint64](Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize * 1KB
    initial_free_ram_bytes = (Get-FreePhysicalBytes).AvailablePhysicalBytes; min_free_ram_bytes = $cutoff
    max_minutes = $MaxMinutes; memory_auto_stop_disabled = $DisableMemoryAutoStop.IsPresent
    build_profile = $Build; console_profile = $Console
    game_executable = (Resolve-Path -LiteralPath $Exe).Path; game_directory = $env:VICTORY_GAME_DIR
    config_file = $configPath; data_directory_override = $env:VICTORY_DATA_DIR; cache_directory_override = $env:VICTORY_CACHE_DIR
    project_executable = if (Test-Path -LiteralPath $projectExe) { (Resolve-Path -LiteralPath $projectExe).Path } else { $null }
    project_executable_sha256 = $projectExeHash; display_mode = '1920x1080 (play.ps1 default)'
    shader_hash_filter = @($ShaderHashes | ForEach-Object { $_.ToLowerInvariant() })
    wave_translation_diagnostics = $env:VICTORY_WAVE_WHY -eq '1'
    process_metrics = 'working set, private bytes, process I/O, CPU'; gpu_metrics = 'process dedicated/shared memory, max 3D engine'; disk_metrics = 'system physical disk read/write bytes/sec'
    process_csv = $csv; gpu_csv = $gpuCsv; gpu_error_csv = $gpuErrorCsv; runtime_stdout = $consoleOut; runtime_stderr = $consoleErr
}
$metadata | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $metadataJson -Encoding UTF8

$play = Join-Path $PSScriptRoot 'play.ps1'
$powershell = Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
if (-not (Test-Path -LiteralPath $powershell)) {
    $powershell = (Get-Command powershell.exe -ErrorAction SilentlyContinue).Source
}
if (-not $powershell) { throw 'Windows PowerShell (powershell.exe) não encontrado para iniciar o controlador.' }

# Start the GPU/disk sampler before the game. Starting a PowerShell job after
# the runtime is found costs several seconds, which can miss every sample in a
# short, memory-limited startup. The job waits for this session's process.
$sessionStartFileTime = $started.ToFileTimeUtc()
$gpuJob = Start-Job -ArgumentList $sessionStartFileTime, $gpuCsv, $gpuErrorCsv -ScriptBlock {
    param([long]$StartedFileTime, [string]$Path, [string]$ErrorPath)
    $diskReadPath = $null; $diskWritePath = $null
    $script:errorCounts = @{}
    function Write-SampleError([string]$Source, [string]$Message) {
        $script:errorCounts[$Source] = 1 + [int]$script:errorCounts[$Source]
        if ($script:errorCounts[$Source] -gt 3) { return }
        [pscustomobject]@{ timestamp = (Get-Date).ToString('o'); source = $Source; error = $Message } |
            Export-Csv -LiteralPath $ErrorPath -NoTypeInformation -Append
    }
    try {
        $diskPaths = (Get-Counter -ListSet PhysicalDisk -ErrorAction Stop).PathsWithInstances |
            Where-Object { $_ -match '\\PhysicalDisk\(_Total\)\\' }
        $diskReadPath = $diskPaths | Where-Object { $_ -match '(?i)(read.*bytes|bytes.*read|leitura.*bytes|bytes.*leitura)' } | Select-Object -First 1
        $diskWritePath = $diskPaths | Where-Object { $_ -match '(?i)(write.*bytes|bytes.*write|gravação.*bytes|bytes.*gravação)' } | Select-Object -First 1
    } catch { Write-SampleError 'disk-counter-discovery' $_.Exception.Message }

    $sessionStart = [datetime]::FromFileTimeUtc($StartedFileTime).ToLocalTime().AddSeconds(-2)
    $target = $null
    while (-not $target) {
        $target = Get-Process victory, P1nkman -ErrorAction SilentlyContinue |
            Where-Object { try { $_.StartTime -ge $sessionStart } catch { $false } } |
            Sort-Object StartTime | Select-Object -First 1
        if (-not $target) { Start-Sleep -Milliseconds 100 }
    }
    $ProcessId = $target.Id

    while (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue) {
        $time = Get-Date
        try {
            $samples = (Get-Counter '\GPU Process Memory(*)\Dedicated Usage', '\GPU Process Memory(*)\Shared Usage', '\GPU Engine(*)\Utilization Percentage' -ErrorAction Stop).CounterSamples
            $mine = @($samples | Where-Object { $_.Path -match "pid_$ProcessId(?:_|\\)" })
            $dedicated = ($mine | Where-Object { $_.Path -match 'dedicated usage$' } | Measure-Object -Property CookedValue -Sum).Sum
            $shared = ($mine | Where-Object { $_.Path -match 'shared usage$' } | Measure-Object -Property CookedValue -Sum).Sum
            $engines = @($mine | Where-Object { $_.Path -match 'engtype_3D' } | ForEach-Object { $_.CookedValue })
            $gpu3d = if ($engines.Count) { ($engines | Measure-Object -Maximum).Maximum } else { 0 }
            $diskRead = [double]0; $diskWrite = [double]0
            if ($diskReadPath -or $diskWritePath) {
                $diskCounters = @($diskReadPath, $diskWritePath) | Where-Object { $_ }
                try {
                    $diskSamples = (Get-Counter -Counter $diskCounters -ErrorAction Stop).CounterSamples
                    $diskRead = ($diskSamples | Where-Object { $_.Path -eq $diskReadPath } | Measure-Object -Property CookedValue -Sum).Sum
                    $diskWrite = ($diskSamples | Where-Object { $_.Path -eq $diskWritePath } | Measure-Object -Property CookedValue -Sum).Sum
                } catch { Write-SampleError 'disk-counters' $_.Exception.Message }
            }
            [pscustomobject]@{
                timestamp = $time.ToString('o'); pid = $ProcessId
                # GPU memory counters can exceed Int32; force floating-point
                # overload resolution before converting the nonnegative result.
                dedicated_bytes = [uint64]([math]::Max([double]0.0, [double]$dedicated))
                shared_bytes = [uint64]([math]::Max([double]0.0, [double]$shared))
                gpu_3d_percent = [math]::Round($gpu3d, 1)
                system_disk_read_bytes_sec = [math]::Round([math]::Max([double]0.0, [double]$diskRead))
                system_disk_write_bytes_sec = [math]::Round([math]::Max([double]0.0, [double]$diskWrite))
            } | Export-Csv -LiteralPath $Path -NoTypeInformation -Append
        } catch { Write-SampleError 'gpu-counters' $_.Exception.Message }
        Start-Sleep -Seconds 1
    }
}
$quotedPlay = '"' + $play.Replace('"', '\"') + '"'
$quotedExe = '"' + (Resolve-Path -LiteralPath $Exe).Path.Replace('"', '\"') + '"'
$arguments = "-NoProfile -ExecutionPolicy Bypass -File $quotedPlay -Exe $quotedExe -Build $Build -Console $Console"
if ($ShaderHashes.Count -gt 0) {
    $shaderCaptureDir = Join-Path $logDir "shader-capture-$stamp"
    New-Item -ItemType Directory -Force -Path $shaderCaptureDir | Out-Null
    $hadShaderSave = Test-Path Env:VICTORY_SHADER_SAVE
    $oldShaderSave = $env:VICTORY_SHADER_SAVE
    $hadShaderHashes = Test-Path Env:VICTORY_SHADER_SAVE_HASHES
    $oldShaderHashes = $env:VICTORY_SHADER_SAVE_HASHES
    $env:VICTORY_SHADER_SAVE = $shaderCaptureDir
    $env:VICTORY_SHADER_SAVE_HASHES = ($ShaderHashes | ForEach-Object { $_.ToLowerInvariant() }) -join ','
    Write-Host "Captura de shaders selecionados: $shaderCaptureDir"
}
try {
    $controller = Start-Process -FilePath $powershell -ArgumentList $arguments -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $consoleOut -RedirectStandardError $consoleErr
} catch {
    if ($gpuJob) {
        Stop-Job -Job $gpuJob -ErrorAction SilentlyContinue
        Remove-Job -Job $gpuJob -Force -ErrorAction SilentlyContinue
        $gpuJob = $null
    }
    throw
} finally {
    if ($ShaderHashes.Count -gt 0) {
        if ($hadShaderSave) { $env:VICTORY_SHADER_SAVE = $oldShaderSave } else { Remove-Item Env:VICTORY_SHADER_SAVE -ErrorAction SilentlyContinue }
        if ($hadShaderHashes) { $env:VICTORY_SHADER_SAVE_HASHES = $oldShaderHashes } else { Remove-Item Env:VICTORY_SHADER_SAVE_HASHES -ErrorAction SilentlyContinue }
    }
}
$runtime = $null
$lastCpu = $null
$lastSample = [Diagnostics.Stopwatch]::StartNew()
$reason = 'runtime terminou normalmente'
$memoryWarning = $false

try {
    while ($true) {
        Start-Sleep -Seconds 1
        $now = Get-Date
        $memoryStatus = Get-FreePhysicalBytes
        $free = [uint64]$memoryStatus.AvailablePhysicalBytes
        if ($runtime -and $runtime.HasExited) { break }
        if (-not $runtime) {
            $runtime = Get-Process victory, P1nkman -ErrorAction SilentlyContinue |
                Where-Object { $_.StartTime -ge $started.AddSeconds(-2) } |
                Sort-Object StartTime | Select-Object -First 1
        }

        # Guard RAM even before discovering the runtime process. Five samples
        # smooth the startup spike; use the net trend, not the steepest single
        # interval, to avoid stopping on a one-sample allocation burst.
        $ramHistory.Enqueue([pscustomobject]@{ Time = $now; Free = $free })
        while ($ramHistory.Count -gt 5) { $null = $ramHistory.Dequeue() }
        $projectedFree = $free
        if ($ramHistory.Count -ge 4) {
            $history = $ramHistory.ToArray()
            $seconds = [math]::Max(0.1, ($history[-1].Time - $history[0].Time).TotalSeconds)
            $declinePerSecond = [math]::Max([double]0.0, [double](($history[0].Free - $history[-1].Free) / $seconds))
            $projectedFree = [math]::Max([double]0.0, [double]([double]$free - 3.0 * [double]$declinePerSecond))
        }

        $working = [uint64]0; $private = [uint64]0; $cpuPct = [double]0
        $readRate = [double]0; $writeRate = [double]0
        if ($runtime) {
            try {
                $runtime.Refresh()
                $working = [uint64]$runtime.WorkingSet64
                $private = [uint64]$runtime.PrivateMemorySize64
                $read = [uint64]$runtime.IOReadBytes
                $write = [uint64]$runtime.IOWriteBytes
                $elapsed = [math]::Max(0.1, $lastSample.Elapsed.TotalSeconds)
                if ($lastCpu) {
                    $cpuPct = [math]::Max(0, (($runtime.CPU - $lastCpu.Cpu) / $elapsed) * 100 / $logicalProcessors)
                    $readRate = [math]::Max(0, ($read - $lastCpu.Read) / $elapsed)
                    $writeRate = [math]::Max(0, ($write - $lastCpu.Write) / $elapsed)
                }
                $lastCpu = @{ Cpu = $runtime.CPU; Read = $read; Write = $write }
            } catch { $runtime = $null }
        }
        $lastSample.Restart()

        [pscustomobject]@{
            timestamp = $now.ToString('o'); elapsed_seconds = [math]::Round($sessionClock.Elapsed.TotalSeconds, 1)
            pid = if ($runtime) { $runtime.Id } else { '' }
            working_set_bytes = $working; private_bytes = $private; free_ram_bytes = $free
            system_commit_bytes = $memoryStatus.CommitBytes; system_commit_limit_bytes = $memoryStatus.CommitLimitBytes
            cpu_percent = [math]::Round($cpuPct, 1); disk_read_bytes_sec = [math]::Round($readRate)
            disk_write_bytes_sec = [math]::Round($writeRate)
            projected_free_ram_3s_bytes = [uint64]$projectedFree
        } | Export-Csv -LiteralPath $csv -NoTypeInformation -Append

        if ($free -le $cutoff -or ($runtime -and $projectedFree -le $cutoff)) {
            $reason = "parada de segurança: RAM livre $([math]::Round($free / 1GB, 2)) GiB; projeção conservadora em 3 s $([math]::Round($projectedFree / 1GB, 2)) GiB; limite $MinFreeGiB GiB"
            if (-not $DisableMemoryAutoStop) {
                if ($runtime) { Stop-Process -Id $runtime.Id -Force -ErrorAction SilentlyContinue }
                if (-not $controller.HasExited) { Stop-Process -Id $controller.Id -Force -ErrorAction SilentlyContinue }
                break
            }
            if (-not $memoryWarning) {
                Write-Warning "RAM atingiu a linha de referência; captura continuará por solicitação explícita: $reason"
                $memoryWarning = $true
            }
        }
        if (-not $runtime -and $controller.HasExited) {
            $reason = 'o iniciador terminou antes de detectar o processo do runtime'
            break
        }
        if ($sessionClock.Elapsed.TotalMinutes -ge $MaxMinutes) {
            $reason = "duração máxima de $MaxMinutes minutos"
            if ($runtime) { Stop-Process -Id $runtime.Id -Force -ErrorAction SilentlyContinue }
            break
        }
    }
} finally {
    if ($runtime -and -not $runtime.HasExited) {
        Stop-Process -Id $runtime.Id -Force -ErrorAction SilentlyContinue
    }
    if (-not $controller.HasExited) {
        Stop-Process -Id $controller.Id -Force -ErrorAction SilentlyContinue
    }
    if ($gpuJob) {
        Wait-Job -Job $gpuJob -Timeout 2 | Out-Null
        if ($gpuJob.State -eq 'Running') { Stop-Job -Job $gpuJob -ErrorAction SilentlyContinue }
        Remove-Job -Job $gpuJob -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "Captura: $csv"
if (Test-Path -LiteralPath $gpuCsv) { Write-Host "GPU: $gpuCsv" }
Write-Host "Saída do runtime: $consoleOut"
Write-Host "Erros do runtime: $consoleErr"
if ($shaderCaptureDir) { Write-Host "Shaders selecionados: $shaderCaptureDir" }
Write-Host "Resultado: $reason"
if ($memoryWarning) { Write-Host 'Aviso: a linha de referência de RAM foi cruzada; revise os CSVs antes de outra execução.' }
