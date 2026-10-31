# Builds the SnipDo extension (snipdo/dist/ContextAI.pbar) that opens ContextAI for the selected text.
# Run: powershell -ExecutionPolicy Bypass -File snipdo\build.ps1, then open the .pbar file to install it.
#
# By default the button presses ContextAI's global shortcut (Ctrl+Alt+Shift+F8) and ContextAI copies the
# selection itself. With -PowerShell the buttons send the text over a named pipe instead; SnipDo runs
# PowerShell actions only in its Pro version.
# Test a PowerShell script the way SnipDo runs it (its param block + the script, in-process):
#   [powershell]::Create().AddScript($snipDoParams + $script).AddParameter('PLAIN_TEXT', 'text').Invoke()
param([switch] $PowerShell)
$ErrorActionPreference = 'Stop'

# -PowerShell: one SnipDo button per entry; `menu` opens the full ContextAI action bar with its submenus.
# Direct buttons can be added for: translate, explain, summarize, edit, grammar, ask, read, translate-read,
# e.g. @{ action = 'translate'; title = 'Преведи' }.
$actions = @(
    @{ action = 'menu'; title = 'ContextAI'; icon = $true }
)

# Sends {"action", "text"} as one JSON line to ContextAI's named pipe and waits for "ok" or "error: ...".
# Each run overwrites %TEMP%\contextai-snipdo.log (inside SnipDo's package folder when it is virtualized).
$scriptBody = @'
$ErrorActionPreference = 'Stop'
$logFile = Join-Path ([IO.Path]::GetTempPath()) 'contextai-snipdo.log'
function Write-Log([string] $Message) {
    try {
        Add-Content -LiteralPath $logFile -Value ("{0:HH:mm:ss.fff} {1}" -f (Get-Date), $Message) -Encoding UTF8
    } catch {}
}
try { Set-Content -LiteralPath $logFile -Value '' -Encoding UTF8 } catch {}
Write-Log "action=$Action textLength=$(([string]$PLAIN_TEXT).Length) ps=$($PSVersionTable.PSVersion) host=$([Diagnostics.Process]::GetCurrentProcess().Path)"

$pipeName = 'contextai-' + ($env:USERNAME -replace '[^A-Za-z0-9._-]', '')
$request = (@{ action = $Action; text = [string]$PLAIN_TEXT } | ConvertTo-Json -Compress) + "`n"

function Connect-ContextAI([int] $TimeoutMs) {
    $pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $pipe.Connect($TimeoutMs)
        return $pipe
    } catch {
        Write-Log "connect to \\.\pipe\$pipeName failed: $($_.Exception.Message)"
        $pipe.Dispose()
        return $null
    }
}

try {
    $pipe = Connect-ContextAI 300
    if (-not $pipe) {
        $pathFile = Join-Path $env:APPDATA 'com.contextai.desktop\contextai-path.txt'
        $exe = if (Test-Path -LiteralPath $pathFile) { (Get-Content -LiteralPath $pathFile -Raw).Trim() }
        if (-not $exe -or -not (Test-Path -LiteralPath $exe)) {
            $exe = Join-Path $env:LOCALAPPDATA 'ContextAI\contextai.exe'
        }
        if (-not (Test-Path -LiteralPath $exe)) {
            throw 'ContextAI не е намерен. Стартирайте го веднъж ръчно.'
        }
        Write-Log "starting $exe"
        # Explorer starts ContextAI outside SnipDo's app container, so its settings are not virtualized.
        Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') -ArgumentList "`"$exe`""
        $pipe = Connect-ContextAI 15000
        if (-not $pipe) {
            throw 'ContextAI не отговаря.'
        }
    }
    try {
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        $writer = New-Object System.IO.StreamWriter($pipe, $utf8)
        $writer.Write($request)
        $writer.Flush()
        $reply = (New-Object System.IO.StreamReader($pipe, $utf8)).ReadLine()
    } finally {
        $pipe.Dispose()
    }
    Write-Log "reply: $reply"
    if ($reply -ne 'ok') {
        throw ($reply -replace '^error: ', '')
    }
} catch {
    Write-Log "error: $($_.Exception.Message)"
    Write-Error $_.Exception.Message
}
'@

$root = $PSScriptRoot
$stage = Join-Path $root 'dist\stage'
$output = Join-Path $root 'dist\ContextAI.pbar'
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -LiteralPath (Join-Path $root '..\src-tauri\icons\64x64.png') -Destination (Join-Path $stage 'icon.png')

# PowerShell 5.1 reads scripts without a BOM as ANSI, which would break the Cyrillic messages.
$utf8Bom = New-Object System.Text.UTF8Encoding($true)
$manifestActions = if (-not $PowerShell) {
    # SendKeys syntax for Ctrl+Alt+Shift+F8, ContextAI's global shortcut for the action bar.
    # A letter key would be typed as text under a Cyrillic layout.
    [ordered]@{ title = 'ContextAI'; icon = 'icon.png'; keyCombo = '^%+{F8}' }
} else {
    foreach ($entry in $actions) {
        $file = "$($entry.action).ps1"
        # No param() block: SnipDo prepends its own (with $PLAIN_TEXT), and a second one is a runtime error.
        $script = "`$Action = '$($entry.action)'`r`n" + ($scriptBody -replace "`r?`n", "`r`n")
        [IO.File]::WriteAllText((Join-Path $stage $file), $script, $utf8Bom)
        $item = [ordered]@{ title = $entry.title; powershellFile = $file }
        if ($entry.icon) { $item.icon = 'icon.png' }
        $item
    }
}
$manifest = [ordered]@{
    name = 'ContextAI'
    identifier = 'com.contextai.snipdo'
    icon = 'icon.png'
    actions = @($manifestActions)
}
[IO.File]::WriteAllText(
    (Join-Path $stage 'extension.json'),
    ($manifest | ConvertTo-Json -Depth 4),
    (New-Object System.Text.UTF8Encoding($false))
)

# Compress-Archive only writes .zip, so the archive is renamed afterwards.
$zip = [IO.Path]::ChangeExtension($output, '.zip')
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
Move-Item -LiteralPath $zip -Destination $output -Force
Remove-Item -LiteralPath $stage -Recurse -Force
Write-Host "Built $output"
