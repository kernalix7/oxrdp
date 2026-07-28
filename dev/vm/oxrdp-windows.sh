#!/usr/bin/env bash
# oxrdp's own Windows guest — a development and validation target owned by this project.
#
# Why a dedicated guest: oxrdp needs a real Windows to capture real application windows, but it
# does not need RDP or winpodx. Borrowing winpodx's guest meant its port mappings (fixed for
# winpodx's own agent, SMB and web viewer) had no room for oxagent's listener, and changing
# them would mean reconfiguring another project's running container. This guest is ours: the
# ports, the credentials and the lifecycle are all under this repo's control.
#
# The guest is dockur/windows (QEMU + KVM). File transfer uses dockur's `/data` mount, which
# appears inside the guest as a network share — no SMB mounting from the host required.
#
# The guest also starts itself: dockur's OEM folder (bind-mounted at /oem) is copied to
# C:\OEM and its install.bat is run once, automatically, during the final step of the
# unattended Windows install (dockur's own FirstLogonCommands, last in the sequence — see
# https://github.com/dockur/windows readme, "How do I run a command after installation?").
# dev/vm/oem/install.bat copies the agent from the shared folder and registers the logon
# Scheduled Task described in docs/design/agent-runtime.md. This only takes effect for a guest
# created AFTER dev/vm/oem/ existed — it is baked into the Windows image at install time, so it
# cannot retroactively affect an already-installed guest. Run `push` before `up` on a fresh
# guest so the files are there when install.bat looks for them.
#
# Bind mounts carry `:Z` so SELinux relabels them for the container; without it dockur fails at
# startup with "Storage folder (/storage) is not writeable".
#
#   dev/vm/oxrdp-windows.sh up       create and start the guest (first run installs Windows)
#   dev/vm/oxrdp-windows.sh status   container state, ports, and whether oxagent is answering
#                                    (on success also prints the TLS pin and the client command)
#   dev/vm/oxrdp-windows.sh logs     follow the installer / boot log
#   dev/vm/oxrdp-windows.sh push     copy the built oxagent.exe + config into the shared folder
#   dev/vm/oxrdp-windows.sh down     stop the guest (keeps the disk)
#   dev/vm/oxrdp-windows.sh destroy  stop and DELETE the disk image (asks first)
set -euo pipefail

NAME="${OXRDP_VM_NAME:-oxrdp-windows}"
# Loopback-only: this guest has no business being reachable from the network.
HOST_IP="127.0.0.1"
AGENT_PORT="${OXRDP_AGENT_PORT:-7644}"   # oxagent's listener, forwarded host -> guest
WEB_PORT="${OXRDP_WEB_PORT:-8010}"       # dockur's web viewer, for install progress and a desktop
RDP_PORT="${OXRDP_RDP_PORT:-3391}"       # plain RDP, only as an escape hatch for manual setup
# Sized to coexist with whatever else is running: the host had ~16G free with winpodx's guest up.
RAM_SIZE="${OXRDP_VM_RAM:-6G}"
CPU_CORES="${OXRDP_VM_CPUS:-4}"
DISK_SIZE="${OXRDP_VM_DISK:-64G}"
WIN_VERSION="${OXRDP_WIN_VERSION:-11}"
USERNAME="${OXRDP_VM_USER:-oxrdp}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VM_DIR="${OXRDP_VM_DIR:-$REPO/.vm}"
STORAGE="$VM_DIR/storage"
SHARED="$VM_DIR/shared"
PASSWORD_FILE="$VM_DIR/password"
OEM_DIR="$REPO/dev/vm/oem"                # checked into git, unlike STORAGE/SHARED under .vm
OEM_LOG="${OXRDP_OEM_LOG:-Y}"              # capture install.bat's output to C:\OEM\install.log

# SHA-256 of zero bytes — what a digest pipeline prints when an earlier stage produced nothing.
# Never a real pin; see agent_pin().
EMPTY_SHA256="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

runtime() { command -v podman >/dev/null && echo podman || echo docker; }
RT="$(runtime)"

ensure_dirs() {
  mkdir -p "$STORAGE" "$SHARED"
  if [[ ! -f "$PASSWORD_FILE" ]]; then
    # Generated once and kept out of git; never passed on a command line by anything but this
    # script's own `run` invocation.
    head -c 18 /dev/urandom | base64 | tr -d '\n/+=' > "$PASSWORD_FILE"
    chmod 600 "$PASSWORD_FILE"
    echo "generated a guest password at $PASSWORD_FILE"
  fi
}

cmd_up() {
  ensure_dirs
  if "$RT" container exists "$NAME" 2>/dev/null || "$RT" ps -a --format '{{.Names}}' | grep -qx "$NAME"; then
    echo "container $NAME already exists; starting it"
    "$RT" start "$NAME"
  else
    if [[ ! -f "$SHARED/oxagent.exe" ]]; then
      echo "warning: $SHARED has no oxagent.exe yet — install.bat will find nothing to stage" >&2
      echo "          run '$0 push' first, or this guest will come up without the agent" >&2
    fi
    echo "creating $NAME (Windows $WIN_VERSION, ${RAM_SIZE} RAM, ${CPU_CORES} cores, ${DISK_SIZE} disk)"
    echo "first boot downloads and installs Windows unattended — expect 10-30 minutes"
    "$RT" run -d --name "$NAME" \
      -e "VERSION=$WIN_VERSION" \
      -e "RAM_SIZE=$RAM_SIZE" \
      -e "CPU_CORES=$CPU_CORES" \
      -e "DISK_SIZE=$DISK_SIZE" \
      -e "USERNAME=$USERNAME" \
      -e "PASSWORD=$(cat "$PASSWORD_FILE")" \
      -e "USER_PORTS=$AGENT_PORT" \
      -e "LOG=$OEM_LOG" \
      -p "$HOST_IP:$WEB_PORT:8006" \
      -p "$HOST_IP:$AGENT_PORT:$AGENT_PORT/tcp" \
      -p "$HOST_IP:$RDP_PORT:3389/tcp" \
      -v "$STORAGE:/storage:Z" \
      -v "$SHARED:/data:Z" \
      -v "$OEM_DIR:/oem:Z" \
      --device=/dev/kvm \
      --cap-add NET_ADMIN \
      --stop-timeout 120 \
      docker.io/dockurr/windows
  fi
  echo
  cmd_status
  echo
  echo "watch the install:  $RT logs -f $NAME     (or open http://$HOST_IP:$WEB_PORT)"
}

# Whether oxagent is running cannot be settled by connect(): dockur's port forwarding completes
# the TCP handshake even when nothing inside Windows is listening, so a bare "open" proves
# nothing. It also cannot be settled by poking the socket with a deliberately malformed TLS
# record — that was tried against this project's own guest and reported the agent as NOT running
# while it was demonstrably listening and had logged the probe itself. rustls does not answer a
# truncated handshake with an alert; it waits for the rest of the record it was promised. A
# healthy agent therefore looks identical to a dead one.
#
# The only question with a trustworthy answer is the one a client actually asks: complete a real
# TLS handshake. openssl does that, and the certificate that comes back yields the very SPKI pin
# the client must be given — so `status` can report the pin as well, and nothing has to be run
# inside the guest to learn it.
#
# Echoes the pin. Exit 0 = TLS answered, 1 = TCP open but no TLS, 2 = TCP connect failed.
agent_pin() {
  local ip="$1" port="$2"
  # Run the connect in a real subprocess: bash prints its own "connection refused" diagnostic
  # from the redirection code, which has been observed to ignore a same-command `2>/dev/null`
  # when the fd is opened with `exec` in the current shell.
  timeout 4 bash -c 'exec 9<>"/dev/tcp/$1/$2"' _ "$ip" "$port" 2>/dev/null || return 2
  # Take the public key first and check it separately, rather than running one long pipeline:
  # every stage after `s_client` fails *quietly* when the handshake produced no certificate, and
  # `dgst` at the end of that pipeline happily hashes nothing at all and prints a perfectly
  # well-formed digest. A plain TCP listener that never speaks TLS was reported as a healthy
  # agent, with EMPTY_SHA256 offered as its pin.
  local pubkey
  pubkey="$(timeout 10 openssl s_client -connect "$ip:$port" </dev/null 2>/dev/null \
            | openssl x509 -pubkey -noout 2>/dev/null)"
  [[ -n "$pubkey" ]] || return 1
  local pin
  pin="$(printf '%s\n' "$pubkey" | openssl pkey -pubin -outform der 2>/dev/null \
         | openssl dgst -sha256 -hex 2>/dev/null | awk '{ print $NF }')"
  # Second belt for the same class of failure, in case some later stage empties out.
  [[ -n "$pin" && "$pin" != "$EMPTY_SHA256" ]] || return 1
  printf '%s\n' "$pin"
}

cmd_status() {
  "$RT" ps -a --filter "name=^${NAME}$" --format '{{.Names}}  {{.Status}}  {{.Ports}}' || true
  printf 'oxagent (port %s): ' "$AGENT_PORT"
  if ! command -v openssl >/dev/null 2>&1; then
    echo "cannot tell without openssl — install it for a real answer"
  else
    local pin
    local probe_rc=0
    pin="$(agent_pin "$HOST_IP" "$AGENT_PORT")" || probe_rc=$?
    case "$probe_rc" in
      0)
        echo "responding — TLS handshake completed"
        echo "  pin: $pin"
        echo "  connect: cargo run -p oxclient -- $HOST_IP:$AGENT_PORT \\"
        echo "             --pin $pin --token-file $SHARED/oxagent-token.txt"
        ;;
      2) echo "unreachable — container down, or the port isn't forwarded" ;;
      *) echo "port open but no TLS answer — oxagent is not running (the forwarded port" \
              "accepts connections even with nothing listening in the guest)" ;;
    esac
  fi
  [[ -f "$PASSWORD_FILE" ]] && echo "guest login: $USERNAME / $(cat "$PASSWORD_FILE")"
  echo "shared folder (host): $SHARED"
  echo "  -> inside the guest this appears as a network share; dockur maps it to \\\\host.lan\\Data"
}

# Stage the agent for the guest. The guest picks the files up from the shared folder; this
# script deliberately does not try to execute anything inside the guest, because a reliable
# remote-exec channel is exactly what this guest does not have yet.
cmd_push() {
  ensure_dirs
  local exe="$REPO/target/x86_64-pc-windows-gnu/debug/oxagent.exe"
  if [[ ! -f "$exe" ]]; then
    echo "no agent binary; build it first:" >&2
    echo "  cargo build -p oxagent --target x86_64-pc-windows-gnu" >&2
    exit 1
  fi

  local token_file="$SHARED/oxagent-token.txt"
  if [[ ! -f "$token_file" ]]; then
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$token_file"
    chmod 600 "$token_file"
    echo "generated an agent token at $token_file"
  fi

  cp "$exe" "$SHARED/oxagent.exe"
  # The guest's address comes from DHCP and is not knowable when this config is written — with
  # passt networking it is not the fixed 10.0.2.15 that QEMU's older user-mode networking used.
  # A wildcard is therefore both necessary and, here, no less safe than the port forward
  # itself: the only path to this guest is the one host port mapped above. The agent refuses a
  # wildcard unless it is asked for explicitly, so ask for it explicitly.
  cat > "$SHARED/oxagent.conf" <<CONF
# oxagent configuration (staged from the host by dev/vm/oxrdp-windows.sh)
bind = 0.0.0.0:$AGENT_PORT
allow_wildcard_bind = true
token_path = oxagent-token.txt
cert_path = oxagent-cert.pem
key_path = oxagent-key.pem
target_fps = 30
max_frames_in_flight = 2
CONF

  echo "staged into $SHARED:"
  ls -la "$SHARED"
  cat <<'NEXT'

A guest created after dev/vm/oem/ existed stages these files and starts oxagent itself — see
dev/vm/oem/install.bat — so on a fresh `up` there is nothing left to do here but wait for
`status` to report the agent responding. `push` after that only matters again for staging a
*rebuilt* binary: install.bat runs once, at first boot, so it does not pick up a later push.

If the guest predates dev/vm/oem/, or install.bat failed (check C:\OEM\install.log via the web
viewer), do this by hand instead:

    net use Z: \\host.lan\Data
    mkdir C:\oxrdp && copy Z:\oxagent.* C:\oxrdp\ && copy Z:\oxagent-token.txt C:\oxrdp\
    cd C:\oxrdp
    .\oxagent.exe --config oxagent.conf

Then run `status` from the host: it completes a real TLS handshake, and prints both the pin and
the exact oxclient command to paste. The pin does not have to be read out of the guest — it is
derived from the certificate the agent presents, and `status` has been checked to agree with
the agent's own `--print-pin` output.
NEXT
}

cmd_down() { "$RT" stop "$NAME" && echo "stopped (disk kept at $STORAGE)"; }

cmd_destroy() {
  read -rp "delete container $NAME AND its disk at $STORAGE? [y/N] " reply
  [[ "$reply" == "y" ]] || { echo "cancelled"; exit 1; }
  "$RT" rm -f "$NAME" 2>/dev/null || true
  rm -rf "$STORAGE"
  echo "removed"
}

case "${1:-}" in
  up) cmd_up ;;
  status) cmd_status ;;
  logs) "$RT" logs -f "$NAME" ;;
  push) cmd_push ;;
  down) cmd_down ;;
  destroy) cmd_destroy ;;
  *) sed -n '2,31p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 2 ;;
esac
