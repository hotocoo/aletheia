# Attach an interactive terminal to Aletheia's serial console over a VirtualBox host pipe.
#
# The VirtualBox window is OUTPUT ONLY: Aletheia draws to the GOP framebuffer it took from the
# firmware, but its console READS from the UART (REQ-CON-002, ADR-045) and there is no PS/2 keyboard
# driver. So a machine booted with `--uart-mode1 server \\.\pipe\aletheia` shows you a prompt in the
# VM window that only this pipe can type at.
#
# docs/VIRTUALBOX.md §3 points Windows users at PuTTY's Serial mode, which works and is one more
# thing to install. This is the dependency-free equivalent — Windows PowerShell only.
#
#   VBoxManage startvm "Aletheia"                 # start it FIRST: the pipe exists only while it runs
#   powershell -ExecutionPolicy Bypass -File scripts/serial-console.ps1
#
# Ctrl-] detaches the terminal and leaves the VM running. `halt` at the prompt stops the machine.

param(
    [string]$Pipe = 'aletheia',
    [int]$TimeoutSeconds = 15
)

$ErrorActionPreference = 'Stop'

Write-Host "connecting to \\.\pipe\$Pipe ..." -ForegroundColor DarkGray
$client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $Pipe, [System.IO.Pipes.PipeDirection]::InOut)
try {
    $client.Connect($TimeoutSeconds * 1000)
} catch {
    Write-Host "could not connect to \\.\pipe\$Pipe" -ForegroundColor Red
    Write-Host "  * is the VM running?   VBoxManage startvm `"Aletheia`"" -ForegroundColor Yellow
    Write-Host "  * is serial 1 wired?   VBoxManage modifyvm `"Aletheia`" --uart1 0x3F8 4 --uart-mode1 server \\.\pipe\$Pipe" -ForegroundColor Yellow
    exit 1
}

Write-Host "connected. type at the prompt; Ctrl-] detaches, 'halt' stops the machine." -ForegroundColor Green
Write-Host "commands: help  arch  uptime  mem  df  ls  stat NAME  cat NAME  write NAME TEXT  rm NAME  echo TEXT  halt" -ForegroundColor DarkGray
Write-Host ("-" * 78) -ForegroundColor DarkGray

$writer = New-Object System.IO.StreamWriter($client)
$writer.AutoFlush = $true

# Reader runs against the same handle on a background task so output arrives while you type.
$buffer = New-Object byte[] 4096
$pending = $client.ReadAsync($buffer, 0, $buffer.Length)

try {
    while ($true) {
        if ($pending.IsCompleted) {
            $n = $pending.Result
            if ($n -le 0) { Write-Host "`n[pipe closed by the VM]" -ForegroundColor DarkGray; break }
            [Console]::Out.Write([System.Text.Encoding]::ASCII.GetString($buffer, 0, $n))
            $pending = $client.ReadAsync($buffer, 0, $buffer.Length)
            continue
        }

        if ([Console]::KeyAvailable) {
            $key = [Console]::ReadKey($true)
            # Ctrl-] detaches without touching the guest.
            if ($key.Modifiers -band [ConsoleModifiers]::Control) {
                if ($key.Key -eq [ConsoleKey]::Oem6) { Write-Host "`n[detached — the VM is still running]" -ForegroundColor DarkGray; break }
            }
            switch ($key.Key) {
                # The line editor wants a bare CR; PowerShell reports Enter as `\r` already.
                'Enter'     { $writer.Write("`r") }
                'Backspace' { $writer.Write([char]8) }
                default     { if ($key.KeyChar -ne "`0") { $writer.Write($key.KeyChar) } }
            }
            continue
        }

        Start-Sleep -Milliseconds 15
    }
} finally {
    $client.Dispose()
}
