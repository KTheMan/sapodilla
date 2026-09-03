[CmdletBinding()]
param(
    [int]$Port = 8922,
    [string]$OutputDirectory = "output/transform"
)

$ErrorActionPreference = "Stop"
$env:NO_COLOR = "true"
$repo = Split-Path -Parent $PSScriptRoot
$output = Join-Path $repo $OutputDirectory
$relativeOutput = ($OutputDirectory -replace '\\', '/').TrimEnd('/')
$fixture = Join-Path $repo "docs/review-evidence/transform-fixture.png"
$session = "sapodilla-transform-verification"

function Invoke-PlaywrightCli {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    & npx --yes --package '@playwright/cli@0.1.19' playwright-cli "-s=$session" @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "playwright-cli failed: $($Arguments -join ' ')"
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
if (-not (Test-Path $fixture)) {
    throw "Transform fixture is missing: $fixture"
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
        Invoke-PlaywrightCli resize 1280 900
        Invoke-PlaywrightCli press 'Control+Shift+U'
        Invoke-PlaywrightCli upload $fixture
        Invoke-PlaywrightCli run-code `
            "async (page) => { await page.waitForTimeout(500); await page.mouse.click(355, 175); await page.mouse.move(355, 175); await page.mouse.down(); await page.mouse.move(555, 375, { steps: 8 }); await page.mouse.up(); await page.waitForTimeout(150); }"
        Invoke-PlaywrightCli screenshot --filename (
            Join-Path $output '01-selected.png'
        )
        Invoke-PlaywrightCli run-code `
            "async (page) => { await page.mouse.move(600, 399); await page.mouse.down(); await page.mouse.move(660, 440, { steps: 8 }); await page.waitForTimeout(150); await page.screenshot({ path: '$relativeOutput/02-resize-active.png' }); await page.mouse.up(); }"
        Invoke-PlaywrightCli run-code `
            "async (page) => { await page.mouse.move(584, 310); await page.mouse.down(); await page.mouse.move(680, 390, { steps: 10 }); await page.waitForTimeout(150); await page.screenshot({ path: '$relativeOutput/03-rotate-active.png' }); await page.mouse.up(); }"
        Invoke-PlaywrightCli snapshot | Out-File `
            -FilePath (Join-Path $output 'accessibility-snapshot.txt') -Encoding utf8
    } finally {
        Invoke-PlaywrightCli close
        Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
    }
} finally {
    Pop-Location
}
