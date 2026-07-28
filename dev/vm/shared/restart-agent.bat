@echo off
rem Restart oxagent in the guest from a freshly pushed binary.
rem
rem Run this from inside the guest as:  \\host.lan\Data\restart-agent.bat
rem
rem Why a file rather than a pasted command: the only channel into this guest during bring-up is
rem an RDP window driven by synthetic key events, and PowerShell one-liners with nested quotes
rem do not survive that trip intact. A batch file on the share is quoted once, here, by a human.
rem
rem dev/vm/oxrdp-windows.sh push copies this alongside oxagent.exe.

set OXDIR=C:\oxrdp
set SHARE=\\host.lan\Data

echo stopping any running agent...
taskkill /IM oxagent.exe /F >nul 2>&1
rem taskkill returns before the process is gone; the copy below fails if we do not wait.
ping -n 3 127.0.0.1 >nul

echo copying the staged binary...
copy /Y "%SHARE%\oxagent.exe" "%OXDIR%\oxagent.exe" >nul
if errorlevel 1 (
  echo FAILED to copy oxagent.exe from %SHARE%
  pause
  exit /b 1
)

echo starting the agent...
rem `start ""` detaches, so this window does not hold the agent open. Stderr carries the pin and
rem every capture error, so it is kept where the host can read it back through the share.
cd /d "%OXDIR%"
start "" /b cmd /c "oxagent.exe --config oxagent.conf 2> agent-err.txt"
ping -n 4 127.0.0.1 >nul

tasklist /FI "IMAGENAME eq oxagent.exe" | find /I "oxagent.exe" >nul
if errorlevel 1 (
  echo agent is NOT running - see %OXDIR%\agent-err.txt
  copy /Y "%OXDIR%\agent-err.txt" "%SHARE%\agent-err.txt" >nul 2>&1
  pause
  exit /b 1
)

echo agent is running.
copy /Y "%OXDIR%\agent-err.txt" "%SHARE%\agent-err.txt" >nul 2>&1
exit /b 0
