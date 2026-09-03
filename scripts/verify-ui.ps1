[CmdletBinding()]
param(
    [int]$Port = 8910,
    [string]$OutputDirectory = "output/playwright"
)

$ErrorActionPreference = "Stop"
# The desktop host sets NO_COLOR=1, while Trunk's clap parser expects a boolean literal.
$env:NO_COLOR = "true"
$repo = Split-Path -Parent $PSScriptRoot
$output = Join-Path $repo $OutputDirectory
$session = "sapodilla-ui-verification"

function Invoke-PlaywrightCli {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    & npx --yes --package '@playwright/cli@0.1.19' playwright-cli "-s=$session" @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "playwright-cli failed: $($Arguments -join ' ')"
    }
}

foreach ($command in @('npx', 'python')) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "$command is required for browser verification."
    }
}

$trunk = Get-Command trunk -ErrorAction SilentlyContinue
if ($trunk) {
    $trunkPath = $trunk.Source
} else {
    $localTrunk = Join-Path $repo '.tools/bin/trunk.exe'
    if (Test-Path $localTrunk) {
        $trunkPath = $localTrunk
    } else {
        throw "trunk is required. Install it or place trunk.exe at .tools/bin/trunk.exe."
    }
}

New-Item -ItemType Directory -Force -Path $output | Out-Null
Push-Location $repo
try {
    & $trunkPath build
    if ($LASTEXITCODE -ne 0) { throw "trunk build failed" }

    $server = Start-Process python `
        -ArgumentList @('-m', 'http.server', $Port, '--directory', (Join-Path $repo 'dist')) `
        -WindowStyle Hidden -PassThru
    try {
        Start-Sleep -Seconds 2
        Invoke-PlaywrightCli open "http://127.0.0.1:$Port"
        foreach ($viewport in @(
            @{ Width = 600; Height = 720 },
            @{ Width = 900; Height = 720 },
            @{ Width = 1024; Height = 768 },
            @{ Width = 1100; Height = 720 },
            @{ Width = 1280; Height = 720 }
        )) {
            Invoke-PlaywrightCli resize $viewport.Width $viewport.Height
            Invoke-PlaywrightCli screenshot --filename (
                Join-Path $output "empty-$($viewport.Width)x$($viewport.Height).png"
            )
        }
        # Verify that Appearance is keyboard-reachable and remains usable at the
        # minimum supported height. A taller capture documents the full palette.
        Invoke-PlaywrightCli press 'Control+,'
        Invoke-PlaywrightCli screenshot --filename (
            Join-Path $output 'appearance-1280x720.png'
        )
        Invoke-PlaywrightCli resize 1280 900
        Invoke-PlaywrightCli run-code `
            "async (page) => { await page.mouse.move(640, 520); await page.mouse.wheel(0, 900); await page.waitForTimeout(150); }"
        Invoke-PlaywrightCli screenshot --filename (
            Join-Path $output 'appearance-1280x900.png'
        )
        Invoke-PlaywrightCli run-code `
            "async (page) => { await page.emulateMedia({ colorScheme: 'dark' }); await page.reload(); await page.setViewportSize({ width: 1280, height: 720 }); }"
        Invoke-PlaywrightCli screenshot --filename (
            Join-Path $output 'dark-1280x720.png'
        )
        Invoke-PlaywrightCli snapshot | Out-File `
            -FilePath (Join-Path $output 'accessibility-snapshot.txt') -Encoding utf8
    } finally {
        Invoke-PlaywrightCli close
        Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
    }
} finally {
    Pop-Location
}
