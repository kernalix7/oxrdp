@echo off
rem A repaint benchmark at a fixed rate with a fixed amount of change per frame.
rem
rem Rate matters as much as constancy, and cmd cannot pace below a second: `timeout /t 1` gave 61
rem frames in a 60 s run where ~400 were needed to tell a clustered latency tail from a spread one,
rem and every frame arrived isolated rather than back-to-back. `ping -w` as a sub-second sleep was
rem worse — its actual delay depends on how the unreachable address fails.
rem
rem PowerShell's Start-Sleep takes milliseconds and means it, so this repaints ten times a second
rem without spinning. Do not "simplify" this back into a cmd loop.
start "" powershell -NoExit -Command "while ($true) { Write-Host ('OXRDP BENCH ################################################################ ' + (Get-Random)); Start-Sleep -Milliseconds 100 }"
exit /b 0
