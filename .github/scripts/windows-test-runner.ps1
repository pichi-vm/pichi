# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# CI launcher for commands requiring independent child lifetimes.
# WMI is CI infrastructure only. The product uses native Job breakaway and
# still refuses a caller Job that forbids it. The pinned Cargo predates its
# breakaway fix (rust-lang/cargo#17527), so execute built tests outside its Job.
$ErrorActionPreference = 'Stop'

if ($args.Count -eq 2 -and $args[0] -eq '--request') {
    $configuration = Get-Content -LiteralPath $args[1] -Raw | ConvertFrom-Json
    foreach ($entry in $configuration.environment.PSObject.Properties) {
        [Environment]::SetEnvironmentVariable($entry.Name, [string]$entry.Value, 'Process')
    }
    $code = 1
    try {
        Set-Location -LiteralPath $configuration.directory
        $testArguments = @($configuration.arguments)
        & $configuration.executable @testArguments *>> $configuration.log
        $code = $LASTEXITCODE
    } catch {
        $_ | Out-String | Add-Content -LiteralPath $configuration.log
    } finally {
        [IO.File]::WriteAllText($configuration.result, [string]$code)
    }
    exit $code
}

if ($args.Count -lt 1) { throw 'Expected an executable' }
$executable = (Resolve-Path -LiteralPath $args[0]).Path
$testArguments = @($args | Select-Object -Skip 1)
$root = Join-Path ([IO.Path]::GetTempPath()) ('pichi-test-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $root | Out-Null
$worker = $null
$code = 1
try {
    $environment = @{}
    # Preserve only explicit, non-secret tool and fixture settings.
    foreach ($name in @('PATH', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'TEMP', 'TMP',
                        'CARGO_HOME', 'CARGO_TARGET_DIR', 'RUSTUP_HOME', 'CARGO_NET_GIT_FETCH_WITH_CLI',
                        'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'APPDATA',
                        'MADRILLO_BIN',
                        'RUNNER_TEMP', 'CI', 'GITHUB_ACTIONS',
                        'RUNNER_TRACKING_ID', 'PICHI_TEST_REGISTRY',
                        'PICHI_TEST_BUILD_IMAGE_LAYOUT',
                        'PICHI_TEST_ROOT_FILESYSTEM',
                        'PICHI_TEST_GUEST_ARMA', 'PICHI_TEST_REPLACEMENT_CORIUM')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if ($null -ne $value) { $environment[$name] = $value }
    }
    $requestFile = Join-Path $root 'request.json'
    $resultFile = Join-Path $root 'exit.txt'
    $logFile = Join-Path $root 'test.log'
    @{
        executable = $executable
        arguments = $testArguments
        directory = (Get-Location).Path
        environment = $environment
        result = $resultFile
        log = $logFile
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $requestFile
    $powershell = (Get-Process -Id $PID).Path
    $command = '"{0}" -NoLogo -NoProfile -NonInteractive -File "{1}" --request "{2}"' -f $powershell, $PSCommandPath, $requestFile
    # Escape WMI's provider Job too, and use the test user's desktop for the
    # caller-window closure fixture. Keep the worker handle until it exits.
    $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
        CreateFlags = [uint32]0x01000000
        WinstationDesktop = 'WinSta0\Default'
        ShowWindow = [uint16]1
    }
    $created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $command; ProcessStartupInformation = $startup }
    if ($created.ReturnValue -ne 0) { throw "WMI test-host creation failed: $($created.ReturnValue)" }
    $worker = Get-Process -Id $created.ProcessId -ErrorAction SilentlyContinue
    if ($null -ne $worker) {
        $null = $worker.Handle
        if (-not $worker.WaitForExit(3600000)) { throw 'Native test executable exceeded one hour' }
    }
    if (-not (Test-Path -LiteralPath $resultFile)) { throw 'Windows test host exited without a result' }
    $code = [int](Get-Content -LiteralPath $resultFile -Raw)
} finally {
    if ($null -ne $worker) {
        if (-not $worker.HasExited) {
            $worker.Kill($true)
            $worker.WaitForExit()
        }
        $worker.Dispose()
    }
    if (Test-Path -LiteralPath (Join-Path $root 'test.log')) { Get-Content -LiteralPath (Join-Path $root 'test.log') }
    Remove-Item -LiteralPath $root -Recurse -Force
}
exit $code
