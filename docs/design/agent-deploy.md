# oxagent deployment into the oxrdp Windows guest

Status: current path (automated, OEM folder) written 2026-07-28, verified as far as this
document says and no further. The historical section at the bottom records an earlier, now
superseded investigation into borrowing winpodx's guest; keep it only as a record of what was
ruled out and why.

## Current path: automatic provisioning via dockur's OEM folder

`dev/vm/oxrdp-windows.sh` creates a dedicated dockur/windows guest for this project (see that
script's own header comment for why it is a dedicated guest rather than winpodx's). Until this
change, getting `oxagent.exe` actually *running* inside that guest required a human to open
dockur's web viewer and type commands — dockur exposes no exec API, so there was no other way
in. That contradicted `docs/design/agent-runtime.md`, which specifies the agent is started by a
logon Scheduled Task, not a person.

dockur/windows supports an **OEM folder**: a host directory bind-mounted into the guest whose
`install.bat` runs automatically, once, during Windows setup. This repo now ships one at
`dev/vm/oem/`, and `dev/vm/oxrdp-windows.sh` mounts it for every guest it creates.

### What dockur's documentation actually says

Quoted from dockur/windows' `readme.md` (`https://github.com/dockur/windows`, "How do I run a
command after installation?"), confirmed by fetching the file directly rather than assumed:

> To run a script or include additional files, create a file called `install.bat` and place it
> in a folder together with any files it needs, such as software to be installed.
>
> Then bind that folder in your compose file like this:
>
> ```yaml
> volumes:
>   - ./example:/oem
> ```
>
> The example folder `./example` will be copied to `C:\OEM` and the `install.bat` file inside it
> will be executed during the final step of the automatic installation.

Everything in the plan as originally sketched matched this except one specific: it isn't "the
launcher" that's special about timing — it's that **`install.bat` only ever runs once, baked
into the unattended install itself**, not on every boot. Confirmed by reading dockur's source
(`src/answer.sh`, `src/install.sh`, and the generated `assets/win11x64.xml` unattend template):

- The OEM folder is copied into the Windows image's `$OEM$\$1\OEM` staging area
  (`addFolder()` in `src/install.sh`), which Windows Setup maps to `%SystemDrive%\OEM`.
- The actual trigger is the **last entry** in the unattend answer file's
  `<FirstLogonCommands>` (order 27 of 27 in the Windows 11 template):
  ```xml
  <CommandLine>cmd /C if exist "C:\OEM\install.bat" start "Install" "cmd /C C:\OEM\install.bat"</CommandLine>
  <Description>Execute custom script from the OEM folder if exists</Description>
  ```
  `FirstLogonCommands` run once, at the first interactive logon after OOBE — in this guest's
  case, the autologon account dockur creates (`USERNAME` from `dev/vm/oxrdp-windows.sh`, which
  the unattend template also puts in the local `Administrators` group). Several of the
  commands immediately before it write to `HKCU`, which only resolves correctly for a logged-on
  user's own hive — confirming these commands run *as* that user, in *that user's* interactive
  session, not as SYSTEM in session 0. That is exactly the session
  `docs/design/agent-runtime.md` requires the agent to run in, so `install.bat` does not need to
  impersonate anyone.
- The two commands immediately before it (order 25–26) already do
  `mklink /d %userprofile%\Desktop\Shared \\host.lan\Data` and
  `net use Z: \\host.lan\Data /persistent:yes` — so by the time `install.bat` runs, the shared
  folder (this project's `.vm/shared`, bind-mounted at `/data`) is already reachable at
  `\\host.lan\Data`, no extra `net use` required. `install.bat` uses that UNC path directly
  rather than depending on the `Z:` drive letter, since only the UNC mapping is documented
  behavior.
- `install.bat` is launched via `start`, i.e. **asynchronously** — Windows Setup does not wait
  for it to finish, and moves on immediately. It does not need to finish before the desktop
  appears; it only needs to eventually finish.
- Setting `LOG=Y` on the container makes dockur redirect the whole of `install.bat`'s
  stdout/stderr to `C:\OEM\install.log` (`enableLog()` in `src/answer.sh`). `LOG` defaults to
  `Y` now for a guest created by `dev/vm/oxrdp-windows.sh`
  (`OXRDP_OEM_LOG`, default `Y`), so a failed run is diagnosable from the web viewer without
  reproducing it.
- dockur normalizes `install.bat`'s *character encoding* (`normalizeBatch()` converts a
  UTF-16 BOM'd file to UTF-8) but does **not** normalize line endings. `dev/vm/oem/install.bat`
  and `dev/vm/oem/register-oxagent-task.ps1` are committed with CRLF line endings deliberately,
  not left to whatever the editing tool defaulted to.

### What's in `dev/vm/oem/`

- **`install.bat`** — copies `oxagent.exe`, `oxagent.conf` and `oxagent-token.txt` from
  `\\host.lan\Data` (staged there by `dev/vm/oxrdp-windows.sh push`, which must run *before* the
  guest is created — `install.bat` only ever gets one shot) into `C:\oxrdp`, restricts that
  directory's ACLs to the guest user + SYSTEM + Administrators (best-effort; this guest has no
  other real user account, but strips the broader inherited ACL anyway), writes a small
  `run-agent.bat` wrapper, and calls `register-oxagent-task.ps1` to register and immediately
  start the Scheduled Task. The token is copied as a file at every step — it is never placed on
  a command line or passed as a script argument.
- **`register-oxagent-task.ps1`** — a separate PowerShell file, not a batch heredoc, specifically
  so the Scheduled Task's settings are ordinary PowerShell rather than batch-escaped strings.
  Registers `OxAgent` with:
  - **Trigger**: `New-ScheduledTaskTrigger -AtLogOn -User <guest user>`.
  - **Principal**: `-LogonType Interactive -RunLevel Limited`. Per Microsoft's Task Scheduler
    schema docs, `InteractiveToken` ("Interactive" in the PowerShell enum) is documented as "the
    task will be run only in an existing interactive session" — this *is* the GUI's "Run only
    when user is logged on" checkbox, and `RunLevel Limited` is "highest privileges" left
    unchecked, matching `docs/design/agent-runtime.md` exactly.
  - **Settings**: `RestartCount 10`, `RestartInterval 1 minute` (restart on failure), and
    `-ExecutionTimeLimit ([TimeSpan]::Zero)`. That last one matters more than it looks: Task
    Scheduler's default execution time limit is **3 days**, after which it kills the task's
    process even if it's perfectly healthy — silently terminating a long-running agent out from
    under a connected client. Microsoft's own schema documentation for `ExecutionTimeLimit`
    states plainly that a value of `PT0S` (`TimeSpan.Zero`) "will enable the task to run
    indefinitely" — confirmed by fetching that page directly, specifically because getting this
    backwards (assuming zero means "terminate immediately") would have been a silent,
    hard-to-diagnose failure of exactly the kind this whole task was trying to avoid.
  - The task is started immediately after registration (`Start-ScheduledTask`), because
    `install.bat` itself runs during the *first* logon and an `AtLogOn` trigger does not fire
    retroactively for the logon already in progress — without this, the agent would not
    actually start until a second logon.
  - The action points at `run-agent.bat`, not `oxagent.exe` directly, so oxagent's stderr (which
    includes its listen address and TLS pin on every normal startup — see
    `crates/oxagent/src/win/mod.rs`) lands in `C:\oxrdp\oxagent.log` instead of a console no
    process ever attaches to, and so the task's action itself needs no argument quoting.

### `dev/vm/oxrdp-windows.sh` changes

- `cmd_up` now mounts `dev/vm/oem` at `/oem:Z` (SELinux relabel, matching the existing
  `/storage` and `/data` mounts) and sets `LOG=Y` on the container by default
  (`OXRDP_OEM_LOG`).
- Creating a *new* guest without `oxagent.exe` already staged in `.vm/shared` now prints a
  warning before creating the container, since `install.bat` will have nothing to copy.
- `cmd_status`'s agent check was rewritten — see below.
- `cmd_push`'s trailing instructions now say a freshly created guest starts the agent on its
  own, and that the manual walkthrough is only needed for a guest that predates `dev/vm/oem/`
  or when `install.bat` failed.

### `status`'s agent check was misleading — fixed, and reproduced against this project's own guest

The task asked to check the current behavior rather than assume it was fine, because "an open
port means the agent" is exactly the kind of claim that's wrong in a way nobody notices. It was
wrong: run against this project's own currently-running guest (`oxrdp-windows`, up for over an
hour, oxagent never started in it), the old check reported:

```
agent port 7644: open (something is listening)
```

That's false — nothing was listening. dockur's guest port forwarding accepts the TCP handshake
on a forwarded port even when nothing inside Windows answers it, so a bare `connect()` succeeding
proves only that the port is *forwarded*, never that anything is behind it. Confirmed directly:
a raw `/dev/tcp` connect against the live guest's port 7644 completed in ~5ms, and an
`openssl s_client` TLS handshake attempt against the same port then sat completely silent
(no `CONNECTED` line, no alert, nothing) until it was killed by its own timeout.

`cmd_status` (in `dev/vm/oxrdp-windows.sh`) now does more than connect: it writes a
deliberately truncated TLS record and waits up to 3 seconds for *any* reply. A real TLS
listener — oxagent included — rejects malformed input almost immediately with a fatal alert; a
forwarded-but-unanswered port just stays silent. Re-run against the same live guest:

```
oxagent (port 7644): port open but silent — oxagent is NOT running (the forwarded port accepts
connections even with nothing listening in the guest; a bare 'open' is not proof)
```

This was checked against all three cases this project's dev machine could produce without a
running oxagent: the live guest (silent — correctly reported "not running"), a definitely-closed
port (correctly reported "unreachable"), and a stub listener that writes one byte back on
connect, standing in for a real TLS server (correctly reported "responding"). It has not been
checked against a *real* running oxagent, because none has been started in this environment —
see "What remains unverified" below.

## What was verified

- `bash -n dev/vm/oxrdp-windows.sh` — clean.
- `dev/vm/oxrdp-windows.sh status` against the actual running `oxrdp-windows` guest, before and
  after every edit in this change, to confirm nothing broke it — the container was never
  stopped, recreated or destroyed.
- The three `cmd_status` outcomes (`responding` / `port open but silent` / `port unreachable`)
  were each reproduced directly, as described above.
- `install.bat`'s control flow — the part that does not require a live Windows guest — was
  tested for real, running the actual committed file under Wine's `cmd.exe` (available in this
  sandbox), with the actual `\\host.lan\Data` reference swapped for a local test directory and
  `powershell.exe` stubbed to a controllable fake:
  - Full success path: all three files present, task registration succeeds → all copies land in
    `C:\oxrdp`, `run-agent.bat` is written with exactly the intended content, exit code `0`.
  - Task registration reports failure → `FATAL` message, exit code `1`.
  - Files missing from the share → three distinct `FATAL` messages (one per missing file),
    registration is still attempted (so the task exists and will start working once someone
    stages the files and logs on again), exit code `1`.
  - This run also caught a real bug before it shipped: the first draft wrote
    `> "file" (block)` to redirect a parenthesized `echo` group into `run-agent.bat`. Under
    real `cmd.exe` that redirection silently does *not* attach to the block — the `echo` output
    went to the console and the file was never created. Fixed to the standard
    `(block) > "file"` form and re-verified byte-for-byte.
- Every PowerShell cmdlet semantic load-bearing in `register-oxagent-task.ps1` was checked
  against Microsoft's own documentation rather than assumed, because no PowerShell interpreter
  was available to execute-test the script in this sandbox:
  - `ExecutionTimeLimit` / `PT0S` = "run indefinitely" (Task Scheduler schema docs).
  - `LogonType Interactive` = `InteractiveToken` = "run only in an existing interactive
    session" (Task Scheduler schema docs) — the "Run only when user is logged on" checkbox.
  - `RunLevel Limited` vs `Highest` = "highest privileges" unchecked vs checked
    (`New-ScheduledTaskPrincipal` docs).
  - `RestartCount` / `RestartInterval` must be set together (`New-ScheduledTaskSettingsSet`
    docs' own example uses both).
- CRLF line endings and absence of a BOM on both `dev/vm/oem/install.bat` and
  `dev/vm/oem/register-oxagent-task.ps1`.

## Verified end to end against the live guest (2026-07-28)

The agent was staged into the running guest by hand (the OEM path below is still untested) and
driven from the host. What this settled:

- **`oxagent.exe` binds and serves with exactly the config `cmd_push` writes.** Running
  `.\oxagent.exe --config oxagent.conf` from `C:\oxrdp` produced
  `TCP 0.0.0.0:7644 LISTENING` in the guest, under session **1** — the interactive session, as
  `agent-runtime.md` requires. The relative `token_path`/`cert_path`/`key_path` resolved against
  the process's working directory as designed.
- **The full client path works**: TLS with SPKI pinning, token auth, version and feature
  negotiation (`protocol v1 codec=1 features=0x3b`), window enumeration
  (`app_id=windowsterminal.exe`, correct title and geometry), and **live frames** —
  1115×628 RAW_BGRA, 2,800,880 bytes each, ~21 fps sustained over the forwarded port. That rate
  is ~470 Mbit/s for one window, which is the concrete argument for the P5 H.264 encoder.
- **`cmd_status`'s "responding" branch against a real oxagent**, after the probe was rewritten
  — the previous malformed-TLS-record probe reported this very agent as "NOT running" while it
  was listening and had logged the probe itself. `status` now completes a real handshake, and
  the pin it derives was checked to match the agent's own `--print-pin` byte for byte.

Two capture bugs were found only by running it, both invisible to the test suite because they
live in `cfg(windows)` code with no Linux counterpart:

1. The frame pool was created with `B8G8R8A8UIntNormalizedSrgb`. WGC accepts only
   `B8G8R8A8UIntNormalized` and `R16G16B16A16Float`, and rejects anything else with a bare
   `E_INVALIDARG` — indistinguishable from a bad window handle.
2. `TryGetNextFrame` reports an empty pool as an **`Err` carrying `S_OK`** (windows-rs turns the
   null frame into an error). Treating that as a failure made the caller destroy and rebuild the
   capture every tick, so the pool never survived long enough to fill and the stream produced
   zero frames while looking busy.

## What remains unverified

- **The actual OEM install path end to end**: dockur copying `dev/vm/oem/` to `C:\OEM` and
  running `install.bat` during a real unattended Windows install. This only takes effect for a
  guest created *after* `dev/vm/oem/` existed — the currently-running guest was created before
  this change and cannot be used to test it without destroying and recreating it, which was out
  of scope for this change (a fresh install takes 10–30 minutes and a good one was already in
  hand for this project's first end-to-end test). Someone needs to run
  `dev/vm/oxrdp-windows.sh push` (with a built `oxagent.exe`), then
  `dev/vm/oxrdp-windows.sh destroy` and `up` on this guest, and watch
  `C:\OEM\install.log` and `dev/vm/oxrdp-windows.sh status` through the install.
- **`Register-ScheduledTask` actually succeeding** on a real Windows 11 guest — the cmdlet
  semantics were checked against documentation, not executed, because no Windows/PowerShell
  environment was available here.
- **The Scheduled Task actually starting `oxagent.exe`.** The agent itself is no longer in
  question — see the section above, where it bound and served under the same config and working
  directory that `run-agent.bat` sets up. What is untested is the task *launching* it.
- **icacls actually landing the intended ACLs** on a real NTFS volume — reasoned from
  documented `icacls` syntax, not executed against real Windows in this sandbox.

## Historical: winpodx-borrowed-guest investigation (superseded)

The rest of this document is what was investigated before this project got its own dedicated
guest (`dev/vm/oxrdp-windows.sh`, see that script's header for why). It recorded an attempt to
deploy `oxagent.exe` into *winpodx's* Windows guest by way of winpodx's own
`windows_exec.run_via_transport` host-to-guest command channel. That channel does not exist for
this project's own guest (dockur exposes no exec API at all, which is the entire reason for the
OEM-folder automation above), so none of the commands below apply to the current deployment
path. Kept only as a record of what was tried and ruled out — do not follow it for oxrdp's own
guest.

### Verified facts (winpodx guest, historical)

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

### Diagnosis (historical)

The live end-to-end test did not reach the protocol layer in this environment. The observed
failures are host/sandbox access failures:

- `socket: Operation not permitted` for loopback TCP probes.
- `AgentClient.health()` failed with `[Errno 1] Operation not permitted`.
- FreeRDP fallback failed before guest result delivery with `error: Unable to allocate instance id`.
- No winpodx RDP password/config file was visible at `~/.config/winpodx/winpodx.toml` during the
  later FreeRDP retry, so the fallback could not authenticate.

Because oxclient never opened a TCP socket, this run did not prove or disprove TLS pin handling,
token authentication, oxagent binding, or Windows.Graphics.Capture behavior. This is the
investigation that led to giving oxrdp its own guest rather than continuing to fight winpodx's
container for a spare port and a working exec channel.
