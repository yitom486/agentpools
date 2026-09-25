param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('codex', 'codex-shared')]
    [string]$Mode,
    [switch]$DetailedProcessTrace
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$binary = Join-Path $repoRoot 'target\debug\examples\batch_add.exe'
if (-not (Test-Path -LiteralPath $binary)) {
    throw "Build the example first: cargo build -p agentpools-acp --example batch_add --all-features"
}
if (-not $env:CODEX_ACP_ENTRY -or -not (Test-Path -LiteralPath $env:CODEX_ACP_ENTRY)) {
    throw 'Set CODEX_ACP_ENTRY to the installed codex-acp dist/index.js.'
}
if (-not $env:CODEX_PATH) {
    $appBin = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Programs\OpenAI\Codex\bin'
    $installedClis = @(Get-ChildItem -LiteralPath $appBin -Filter codex.exe -File -Recurse `
        -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
    if ($installedClis.Count -eq 0) {
        throw 'Cannot find the Codex app CLI. Set CODEX_PATH to a compatible codex.exe.'
    }
    $env:CODEX_PATH = $installedClis[0].FullName
}
if (-not (Test-Path -LiteralPath $env:CODEX_PATH)) {
    throw 'CODEX_PATH does not point to an existing Codex CLI executable.'
}

$loginStatus = (& $env:CODEX_PATH login status 2>&1 | Out-String).Trim()
$isLoggedIn = $loginStatus -match '(?m)^\s*Logged in\b'
if ($LASTEXITCODE -ne 0 -or -not $isLoggedIn) {
    throw "The selected Codex CLI cannot use a stored login in this process: $loginStatus"
}

$outputRoot = Join-Path $repoRoot 'target\agentpools-batch-add-resource'
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$runName = '{0}-{1}' -f $Mode, (Get-Date -Format 'yyyyMMdd-HHmmss')
$stdoutPath = Join-Path $outputRoot "$runName.stdout.txt"
$stderrPath = Join-Path $outputRoot "$runName.stderr.txt"
$summaryPath = Join-Path $outputRoot "$runName.resources.json"

$timer = [Diagnostics.Stopwatch]::StartNew()
$benchmark = Start-Process -FilePath $binary -ArgumentList "--$Mode" `
    -WorkingDirectory $repoRoot -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath

$peakProcessCount = 0
$peakWorkingSet = [long]0
$peakPrivateBytes = [long]0
$peakNames = @()
$peakProcesses = @()
$observedProcesses = @{}
$cpuByPid = @{}

while ($true) {
    $benchmark.Refresh()
    $properties = @('ProcessId', 'ParentProcessId', 'Name')
    if ($DetailedProcessTrace) {
        $properties += @('ExecutablePath', 'CommandLine')
    }
    $processRows = @(Get-CimInstance Win32_Process -Property $properties)
    $children = @{}
    $rowsByPid = @{}
    foreach ($row in $processRows) {
        $rowsByPid[[int]$row.ProcessId] = $row
        $parentId = [int]$row.ParentProcessId
        if (-not $children.ContainsKey($parentId)) {
            $children[$parentId] = [System.Collections.Generic.List[int]]::new()
        }
        $children[$parentId].Add([int]$row.ProcessId)
    }

    $seen = [System.Collections.Generic.HashSet[int]]::new()
    $pending = [System.Collections.Generic.Queue[int]]::new()
    [void]$seen.Add($benchmark.Id)
    $pending.Enqueue($benchmark.Id)
    while ($pending.Count -gt 0) {
        $parentId = $pending.Dequeue()
        if (-not $children.ContainsKey($parentId)) { continue }
        foreach ($childId in $children[$parentId]) {
            if ($seen.Add($childId)) { $pending.Enqueue($childId) }
        }
    }

    $ids = [int[]]@($seen)
    $live = @(Get-Process -Id $ids -ErrorAction SilentlyContinue)
    $workingSet = [long](($live | Measure-Object -Property WorkingSet64 -Sum).Sum)
    $privateBytes = [long](($live | Measure-Object -Property PrivateMemorySize64 -Sum).Sum)
    $currentProcesses = @()
    if ($DetailedProcessTrace) {
        $nowMs = $timer.ElapsedMilliseconds
        foreach ($item in $live) {
            $row = $rowsByPid[[int]$item.Id]
            if ($null -eq $row) { continue }
            $parentId = [int]$row.ParentProcessId
            $parent = $rowsByPid[$parentId]
            $name = [IO.Path]::GetFileNameWithoutExtension([string]$row.Name)
            $commandLine = [string]$row.CommandLine
            $role = if ($name -eq 'node_repl') {
                'codex-node-repl'
            } elseif ($name -eq 'codex-code-mode-host') {
                'codex-code-mode-host'
            } elseif ($name -eq 'codex' -and $commandLine -match '(?i)\bapp-server\b') {
                'codex-app-server'
            } elseif ($commandLine -match '(?i)@agentclientprotocol[\\/]codex-acp[\\/]dist[\\/]index\.js') {
                'codex-acp-node-adapter'
            } elseif ($name -eq 'agentpools-mcp-add') {
                'calculator-mcp'
            } elseif ($name -eq 'node') {
                'node-other'
            } else {
                'other'
            }
            $process = [ordered]@{
                pid = [int]$item.Id
                ppid = $parentId
                name = $name
                parent_name = if ($null -ne $parent) { [IO.Path]::GetFileNameWithoutExtension([string]$parent.Name) } else { 'unknown' }
                executable = if ($row.ExecutablePath) { [IO.Path]::GetFileName([string]$row.ExecutablePath) } else { 'unknown' }
                role = $role
            }
            $currentProcesses += [pscustomobject]$process
            if (-not $observedProcesses.ContainsKey([int]$item.Id)) {
                $observedProcesses[[int]$item.Id] = [ordered]@{
                    pid = [int]$item.Id
                    ppid = $parentId
                    name = $name
                    parent_name = $process.parent_name
                    executable = $process.executable
                    role = $role
                    first_seen_ms = $nowMs
                    last_seen_ms = $nowMs
                }
            } else {
                $observedProcesses[[int]$item.Id]['last_seen_ms'] = $nowMs
            }
        }
    }

    if ($live.Count -gt $peakProcessCount) { $peakProcessCount = $live.Count }
    if ($workingSet -gt $peakWorkingSet) {
        $peakWorkingSet = $workingSet
        $peakNames = @($live | ForEach-Object { $_.ProcessName } | Sort-Object)
        if ($DetailedProcessTrace) { $peakProcesses = @($currentProcesses) }
    }
    if ($privateBytes -gt $peakPrivateBytes) { $peakPrivateBytes = $privateBytes }
    foreach ($item in $live) {
        if ($null -eq $item.CPU) { continue }
        $elapsedCpu = [double]$item.CPU
        if (-not $cpuByPid.ContainsKey($item.Id) -or $elapsedCpu -gt $cpuByPid[$item.Id]) {
            $cpuByPid[$item.Id] = $elapsedCpu
        }
    }

    if ($benchmark.HasExited) { break }
    Start-Sleep -Milliseconds 250
}

$benchmark.WaitForExit()
$timer.Stop()
$summary = [ordered]@{
    mode = $Mode
    user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    codex_cli = $env:CODEX_PATH
    exit_code = $benchmark.ExitCode
    wall_ms = $timer.ElapsedMilliseconds
    peak_process_count = $peakProcessCount
    peak_working_set_mb = [math]::Round($peakWorkingSet / 1MB, 1)
    peak_private_mb = [math]::Round($peakPrivateBytes / 1MB, 1)
    observed_cpu_seconds = [math]::Round((($cpuByPid.Values | Measure-Object -Sum).Sum), 1)
    process_names_at_peak = $peakNames
    stdout = $stdoutPath
    stderr = $stderrPath
}
if ($DetailedProcessTrace) {
    $summary.processes_at_peak = $peakProcesses
    $summary.process_lifetimes = @($observedProcesses.Values | Sort-Object first_seen_ms)
}
$summary | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $summaryPath -Encoding utf8
$summary | ConvertTo-Json -Depth 4
if ($benchmark.ExitCode -ne 0) { exit $benchmark.ExitCode }
