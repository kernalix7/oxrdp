@echo off
REM oxagent bring-up for the oxrdp Windows guest.
REM
REM dockur/windows copies this whole folder to C:\OEM and runs install.bat once, during the
REM final step of the automatic Windows install: it is the last of the unattend answer file's
REM FirstLogonCommands, launched (via "start") in the interactive logon session of the guest's
REM autologon user, which is also a local Administrator. That session is exactly where
REM docs/design/agent-runtime.md requires the agent to run, so this script does not need to
REM impersonate anyone or juggle sessions -- it already IS the agent's user.
REM
REM By the time this runs, dockur has already (per its own earlier FirstLogonCommands) mapped
REM the /data shared folder to \\host.lan\Data and to drive Z:, and relaxed guest SMB auth for
REM it. dev/vm/oxrdp-windows.sh's `push` subcommand stages oxagent.exe, oxagent.conf and
REM oxagent-token.txt into that shared folder (host side: .vm/shared) before this guest is
REM created; this script copies them from there.
REM
REM Set LOG=Y on the container (dev/vm/oxrdp-windows.sh does this by default) to have dockur
REM redirect this whole script's output to C:\OEM\install.log for troubleshooting.

setlocal EnableExtensions

set "SHARE=\\host.lan\Data"
set "DEST=C:\oxrdp"
set "OEMDIR=%~dp0"
set "STAGE_OK=1"

echo [oxrdp] install.bat starting %DATE% %TIME%
echo [oxrdp] source (shared folder): %SHARE%
echo [oxrdp] destination:            %DEST%

if not exist "%DEST%" (
  mkdir "%DEST%"
  if errorlevel 1 (
    echo [oxrdp] FATAL: could not create %DEST%
    exit /b 1
  )
)

if exist "%SHARE%\oxagent.exe" (
  copy /Y "%SHARE%\oxagent.exe" "%DEST%\oxagent.exe" >nul
  if errorlevel 1 (
    echo [oxrdp] FATAL: failed to copy oxagent.exe from %SHARE%
    set "STAGE_OK=0"
  )
) else (
  echo [oxrdp] FATAL: %SHARE%\oxagent.exe not found -- run "dev/vm/oxrdp-windows.sh push" on the host before creating this guest
  set "STAGE_OK=0"
)

if exist "%SHARE%\oxagent.conf" (
  copy /Y "%SHARE%\oxagent.conf" "%DEST%\oxagent.conf" >nul
  if errorlevel 1 (
    echo [oxrdp] FATAL: failed to copy oxagent.conf from %SHARE%
    set "STAGE_OK=0"
  )
) else (
  echo [oxrdp] FATAL: %SHARE%\oxagent.conf not found
  set "STAGE_OK=0"
)

if exist "%SHARE%\oxagent-token.txt" (
  REM A file copy, never a command-line argument: argv is readable by every process on the
  REM machine, and this token authenticates every client connection.
  copy /Y "%SHARE%\oxagent-token.txt" "%DEST%\oxagent-token.txt" >nul
  if errorlevel 1 (
    echo [oxrdp] FATAL: failed to copy oxagent-token.txt from %SHARE%
    set "STAGE_OK=0"
  )
) else (
  echo [oxrdp] FATAL: %SHARE%\oxagent-token.txt not found
  set "STAGE_OK=0"
)

REM Best-effort lockdown: the token (and, after first run, the agent's TLS private key) live
REM here. This guest has no other real user account, but strip inherited ACLs anyway so the
REM directory reads "only this user, SYSTEM, Administrators" rather than "whatever C:\ grants."
icacls "%DEST%" /inheritance:r /grant:r "%USERNAME%:(OI)(CI)F" "SYSTEM:(OI)(CI)F" "Administrators:(OI)(CI)F" >nul
if errorlevel 1 echo [oxrdp] WARNING: icacls could not lock down %DEST% permissions ^(non-fatal^)

REM A tiny wrapper, not the Scheduled Task calling oxagent.exe directly, for two reasons: it
REM keeps the task's own action free of any quoting to get wrong, and it captures stderr -- the
REM agent logs its listen address and TLS pin there on every normal startup -- to a file instead
REM of a console no one will ever attach to.
(
  echo @echo off
  echo cd /d "%DEST%"
  echo oxagent.exe --config oxagent.conf ^>^> oxagent.log 2^>^&1
) > "%DEST%\run-agent.bat"

echo [oxrdp] registering the logon Scheduled Task
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%OEMDIR%register-oxagent-task.ps1" -UserName "%USERNAME%" -AgentDir "%DEST%"
if errorlevel 1 (
  echo [oxrdp] FATAL: Scheduled Task registration failed
  set "STAGE_OK=0"
) else (
  echo [oxrdp] Scheduled Task registered and started
)

if "%STAGE_OK%"=="0" (
  echo [oxrdp] install.bat finished WITH ERRORS -- see above
  exit /b 1
)

echo [oxrdp] install.bat finished OK
exit /b 0
