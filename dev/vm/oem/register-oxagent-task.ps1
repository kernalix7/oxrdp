# Registers (and starts) the logon Scheduled Task that runs oxagent.exe, per the deployment
# model in docs/design/agent-runtime.md. Invoked once by install.bat during the automatic
# Windows install; safe to re-run by hand afterwards (re-registers with -Force).
#
# This is a separate .ps1, rather than a heredoc assembled inside install.bat, specifically so
# the task settings below are ordinary PowerShell syntax instead of batch-escaped strings --
# batch quoting is exactly the kind of thing that fails silently.
param(
    [string]$UserName = $env:USERNAME,
    [string]$AgentDir = 'C:\oxrdp',
    [string]$TaskName = 'OxAgent'
)

$ErrorActionPreference = 'Stop'

try {
    $action = New-ScheduledTaskAction `
        -Execute (Join-Path $AgentDir 'run-agent.bat') `
        -WorkingDirectory $AgentDir

    # AtLogOn scoped to this user, combined with LogonType Interactive below, is the "Run only
    # when user is logged on" checkbox: Task Scheduler's own docs describe InteractiveToken as
    # "the task will be run only in an existing interactive session." That is the whole point
    # of this design -- session 0 cannot capture windows or inject input -- so this is not an
    # incidental setting.
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $UserName

    # RunLevel Limited is "run with highest privileges" left UNCHECKED: the agent does not need
    # elevation, and elevated oxagent would still be unable to see or touch un-elevated windows
    # anyway (Windows' UI Access / UIPI boundary), so elevation would only add attack surface.
    $principal = New-ScheduledTaskPrincipal -UserId $UserName -LogonType Interactive -RunLevel Limited

    $settings = New-ScheduledTaskSettingsSet `
        -MultipleInstances IgnoreNew `
        -RestartCount 10 `
        -RestartInterval (New-TimeSpan -Minutes 1) `
        -ExecutionTimeLimit ([TimeSpan]::Zero)
        # ExecutionTimeLimit defaults to 3 days if left unset -- Task Scheduler would then kill
        # a perfectly healthy long-running agent out from under a client mid-session. Per
        # Microsoft's own schema docs, PT0S (TimeSpan.Zero) is the documented way to say
        # "no limit," not "terminate immediately" -- do not "simplify" this away.

    Register-ScheduledTask `
        -TaskName $TaskName `
        -Action $action `
        -Trigger $trigger `
        -Principal $principal `
        -Settings $settings `
        -Force | Out-Null

    Write-Output "[oxrdp] registered scheduled task '$TaskName' (user=$UserName, dir=$AgentDir)"

    # install.bat runs during the session's *first* logon; the AtLogOn trigger only fires on
    # future logons, not retroactively for the one already in progress. Start it now so the
    # agent comes up immediately instead of only after the next reboot.
    Start-ScheduledTask -TaskName $TaskName
    Write-Output "[oxrdp] started scheduled task '$TaskName'"
}
catch {
    # Deliberately Write-Output, not Write-Error: with $ErrorActionPreference = 'Stop',
    # Write-Error itself raises a terminating error, which would skip the explicit `exit 1`
    # below and leave the real process exit code up to PowerShell's own (inconsistent) default
    # -- and install.bat's whole error-propagation path depends on that exit code being right.
    Write-Output "[oxrdp] ERROR: failed to register/start scheduled task '$TaskName': $_"
    exit 1
}

exit 0
