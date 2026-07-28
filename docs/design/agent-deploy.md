# oxagent deployment into winpodx Windows guest

Status: investigation run on 2026-07-28.

This file records what was actually verified in the live workspace and what was not verified.
Do not treat the unverified commands below as a proven recipe until they are run from a shell
that can open loopback sockets to the winpodx guest.

## Verified facts

- `cargo build -p oxagent --target x86_64-pc-windows-gnu` succeeds and produces
  `target/x86_64-pc-windows-gnu/debug/oxagent.exe`.
- `cargo run -p oxclient -- 127.0.0.1:8765 --pin <64 hex> --token-file <path>` loads the token
  file and then fails at TCP connect in this sandbox with `Operation not permitted (os error 1)`.
- The winpodx source identifies the supported host-to-guest command channel:
  `winpodx.core.windows_exec.run_via_transport(cfg, script, ...)`.
- `run_via_transport` tries the winpodx HTTP guest agent `/exec` first, then falls back to the
  FreeRDP RemoteApp PowerShell path.
- The FreeRDP fallback exposes the host home directory in the guest as `\\tsclient\home`.
- winpodx compose forwards only these relevant guest ports by default:
  - RDP: host `127.0.0.1:<cfg.rdp.port>` to guest `3389`
  - winpodx HTTP agent: host `127.0.0.1:8765` to guest `8765`
  - SMB: host `127.0.0.1:4445` to guest `445`
- No spare oxagent TCP port is exposed by the checked winpodx compose source. To test oxagent
  without reconfiguring the container, the only existing protocol port is `8765`, which is
  normally occupied by winpodx's own HTTP guest agent.

## Commands run

Host-side Windows build:

```bash
cargo build -p oxagent --target x86_64-pc-windows-gnu
```

Result:

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.07s
```

winpodx transport probe:

```bash
XDG_DATA_HOME=/home/kernalix7/Desktop/00_Personal_Project/00G_oxrdp/.tmp/xdg-data \
PYTHONPATH=/home/kernalix7/Desktop/00_Personal_Project/00G_winpodx/src \
python3 - <<'PY'
from winpodx.core.config import Config
from winpodx.core.windows_exec import run_via_transport
cfg = Config.load()
r = run_via_transport(cfg, 'Write-Output "oxrdp-exec-ok"',
                      description='oxrdp-agent-probe', timeout=60)
print('rc', r.rc)
print('stdout', r.stdout.strip())
print('stderr', r.stderr.strip())
PY
```

Result:

```text
FreeRDP-fallback: agent unavailable (/health unreachable: [Errno 1] Operation not permitted); using FreeRDP
FreeRDP-fallback: 'oxrdp-agent-probe' ran over FreeRDP (agent not selected)
WindowsExecError No result file written (FreeRDP rc=1). stderr tail: 'error: Unable to allocate instance id'
```

Loopback socket probes:

```bash
timeout 3 bash -lc '</dev/tcp/127.0.0.1/3390' && echo rdp-open || echo rdp-failed
timeout 3 bash -lc '</dev/tcp/127.0.0.1/4445' && echo smb-open || echo smb-failed
timeout 3 bash -lc '</dev/tcp/127.0.0.1/8765' && echo agent-open || echo agent-failed
```

Result:

```text
bash: socket: Operation not permitted
rdp-failed
bash: socket: Operation not permitted
smb-failed
bash: socket: Operation not permitted
agent-failed
```

Host-side staging:

```bash
mkdir -p .tmp/oxagent-stage
umask 077
openssl rand -hex 32 > .tmp/oxagent-stage/oxagent-token.txt
printf 'bind = 127.0.0.1:8765\ntoken_path = C:\\oxrdp\\oxagent-token.txt\ncert_path = C:\\oxrdp\\oxagent-cert.pem\nkey_path = C:\\oxrdp\\oxagent-key.pem\ntarget_fps = 30\nmax_frames_in_flight = 2\n' > .tmp/oxagent-stage/oxagent.conf
cp target/x86_64-pc-windows-gnu/debug/oxagent.exe .tmp/oxagent-stage/oxagent.exe
ls -lh .tmp/oxagent-stage/oxagent.exe
```

Result:

```text
-rwxr-xr-x. 1 kernalix7 kernalix7 59M Jul 28 11:57 .tmp/oxagent-stage/oxagent.exe
```

Client probe:

```bash
cargo run -p oxclient -- 127.0.0.1:8765 \
  --pin 0000000000000000000000000000000000000000000000000000000000000000 \
  --token-file .tmp/oxagent-stage/oxagent-token.txt
```

Result:

```text
oxclient: Operation not permitted (os error 1)
```

## Unverified deployment path

These commands follow winpodx's own transport design, but were not verified in this sandbox.

1. Build and stage files on the host:

   ```bash
   cargo build -p oxagent --target x86_64-pc-windows-gnu
   mkdir -p .tmp/oxagent-stage
   umask 077
   openssl rand -hex 32 > .tmp/oxagent-stage/oxagent-token.txt
   printf 'bind = 127.0.0.1:8765\ntoken_path = C:\\oxrdp\\oxagent-token.txt\ncert_path = C:\\oxrdp\\oxagent-cert.pem\nkey_path = C:\\oxrdp\\oxagent-key.pem\ntarget_fps = 30\nmax_frames_in_flight = 2\n' > .tmp/oxagent-stage/oxagent.conf
   cp target/x86_64-pc-windows-gnu/debug/oxagent.exe .tmp/oxagent-stage/oxagent.exe
   ```

2. Copy staged files into `C:\oxrdp` through winpodx's FreeRDP `\\tsclient\home` file channel:

   ```bash
   XDG_DATA_HOME=/home/kernalix7/Desktop/00_Personal_Project/00G_oxrdp/.tmp/xdg-data \
   PYTHONPATH=/home/kernalix7/Desktop/00_Personal_Project/00G_winpodx/src \
   python3 - <<'PY'
   from winpodx.core.config import Config
   from winpodx.core.windows_exec import run_via_transport
   cfg = Config.load()
   script = r'''
   $ErrorActionPreference = 'Stop'
   New-Item -ItemType Directory -Force -Path 'C:\oxrdp' | Out-Null
   Copy-Item -Force '\\tsclient\home\Desktop\00_Personal_Project\00G_oxrdp\.tmp\oxagent-stage\oxagent.exe' 'C:\oxrdp\oxagent.exe'
   Copy-Item -Force '\\tsclient\home\Desktop\00_Personal_Project\00G_oxrdp\.tmp\oxagent-stage\oxagent.conf' 'C:\oxrdp\oxagent.conf'
   Copy-Item -Force '\\tsclient\home\Desktop\00_Personal_Project\00G_oxrdp\.tmp\oxagent-stage\oxagent-token.txt' 'C:\oxrdp\oxagent-token.txt'
   Get-ChildItem 'C:\oxrdp' | Select-Object Name,Length | Format-Table -AutoSize
   '''
   r = run_via_transport(cfg, script, description='oxrdp-stage-agent', timeout=90)
   print('rc', r.rc)
   print(r.stdout)
   print(r.stderr)
   PY
   ```

3. Generate or read the agent TLS pin inside the guest:

   ```bash
   XDG_DATA_HOME=/home/kernalix7/Desktop/00_Personal_Project/00G_oxrdp/.tmp/xdg-data \
   PYTHONPATH=/home/kernalix7/Desktop/00_Personal_Project/00G_winpodx/src \
   python3 - <<'PY'
   from winpodx.core.config import Config
   from winpodx.core.windows_exec import run_via_transport
   cfg = Config.load()
   r = run_via_transport(
       cfg,
       r'& C:\oxrdp\oxagent.exe --config C:\oxrdp\oxagent.conf --print-pin',
       description='oxrdp-print-pin',
       timeout=60,
   )
   print('rc', r.rc)
   print(r.stdout.strip())
   print(r.stderr.strip())
   PY
   ```

4. Launch oxagent on the existing forwarded port.

   This temporarily replaces the winpodx HTTP agent on guest port `8765`. The task is delayed so
   the `/exec` response can return before the winpodx agent process is stopped.

   ```bash
   XDG_DATA_HOME=/home/kernalix7/Desktop/00_Personal_Project/00G_oxrdp/.tmp/xdg-data \
   PYTHONPATH=/home/kernalix7/Desktop/00_Personal_Project/00G_winpodx/src \
   python3 - <<'PY'
   from winpodx.core.config import Config
   from winpodx.core.windows_exec import run_via_transport
   cfg = Config.load()
   script = r'''
   $ErrorActionPreference = 'Stop'
   $launch = @'
   Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
     Where-Object { $_.CommandLine -like '*agent.ps1*' } |
     ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
   Start-Sleep -Seconds 1
   Start-Process -FilePath 'C:\oxrdp\oxagent.exe' `
     -ArgumentList @('--config','C:\oxrdp\oxagent.conf') `
     -RedirectStandardOutput 'C:\oxrdp\oxagent.out.log' `
     -RedirectStandardError 'C:\oxrdp\oxagent.err.log' `
     -WindowStyle Hidden
   '@
   Set-Content -Path 'C:\oxrdp\launch-oxagent.ps1' -Value $launch -Encoding UTF8
   $act = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\oxrdp\launch-oxagent.ps1'
   $trg = New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds(5)
   Register-ScheduledTask -TaskName 'OxrdpAgentBringup' -Action $act -Trigger $trg -Force | Out-Null
   Start-ScheduledTask -TaskName 'OxrdpAgentBringup'
   Write-Output 'scheduled OxrdpAgentBringup'
   '''
   r = run_via_transport(cfg, script, description='oxrdp-launch-agent', timeout=60)
   print('rc', r.rc)
   print(r.stdout)
   print(r.stderr)
   PY
   ```

5. Connect from Linux:

   ```bash
   cargo run -p oxclient -- 127.0.0.1:8765 --pin <pin-from-step-3> \
     --token-file .tmp/oxagent-stage/oxagent-token.txt
   ```

6. If the client fails after TCP connect, read `C:\oxrdp\oxagent.err.log` and
   `C:\oxrdp\oxagent.out.log` from the guest before changing code.

## Diagnosis

The live end-to-end test did not reach the protocol layer in this environment. The observed
failures are host/sandbox access failures:

- `socket: Operation not permitted` for loopback TCP probes.
- `AgentClient.health()` failed with `[Errno 1] Operation not permitted`.
- FreeRDP fallback failed before guest result delivery with `error: Unable to allocate instance id`.
- No winpodx RDP password/config file was visible at `~/.config/winpodx/winpodx.toml` during the
  later FreeRDP retry, so the fallback could not authenticate.

Because oxclient never opened a TCP socket, this run does not prove or disprove TLS pin handling,
token authentication, oxagent binding, or Windows.Graphics.Capture behavior.
