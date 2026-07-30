@echo off
rem A repaint benchmark with a *fixed* rate and a *fixed* amount of change per frame.
rem
rem The previous benchmark was `for /L ... echo`, whose output rate follows guest scheduling and
rem whose per-frame delta depends on how much text scrolled. Two runs were therefore not the same
rem workload, and run-to-run variance swamped every effect worth measuring. This one repaints a
rem constant-width line on a timer, so the encoded size per frame stays roughly constant and the
rem rate is set by us rather than by the scheduler.
rem
rem `timeout /t 1` is one second of real waiting, not a busy loop, so the guest is not also being
rem loaded by the thing measuring it.
setlocal enabledelayedexpansion
set LINE=OXRDP BENCH ################################################################
:loop
echo %LINE% !RANDOM!
timeout /t 1 /nobreak >nul
goto loop
