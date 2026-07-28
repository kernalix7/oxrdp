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
# Bind mounts carry `:Z` so SELinux relabels them for the container; without it dockur fails at
# startup with "Storage folder (/storage) is not writeable".
#
#   dev/vm/oxrdp-windows.sh up       create and start the guest (first run installs Windows)
#   dev/vm/oxrdp-windows.sh status   container state, ports, and whether the agent port answers
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
      -p "$HOST_IP:$WEB_PORT:8006" \
      -p "$HOST_IP:$AGENT_PORT:$AGENT_PORT/tcp" \
      -p "$HOST_IP:$RDP_PORT:3389/tcp" \
      -v "$STORAGE:/storage:Z" \
      -v "$SHARED:/data:Z" \
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

cmd_status() {
  "$RT" ps -a --filter "name=^${NAME}$" --format '{{.Names}}  {{.Status}}  {{.Ports}}' || true
  printf 'agent port %s: ' "$AGENT_PORT"
  if timeout 2 bash -c "</dev/tcp/$HOST_IP/$AGENT_PORT" 2>/dev/null; then
    echo "open (something is listening)"
  else
    echo "closed (oxagent not running in the guest yet)"
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
  # The agent refuses a wildcard bind, so it binds the guest's own address. dockur's user-mode
  # networking forwards the host port to the guest, so the guest must listen on all of its
  # interfaces from the host's point of view — 10.0.2.15 is the fixed address QEMU's user
  # networking assigns.
  cat > "$SHARED/oxagent.conf" <<CONF
# oxagent configuration (staged from the host by dev/vm/oxrdp-windows.sh)
bind = 10.0.2.15:$AGENT_PORT
token_path = oxagent-token.txt
cert_path = oxagent-cert.pem
key_path = oxagent-key.pem
target_fps = 30
max_frames_in_flight = 2
CONF

  echo "staged into $SHARED:"
  ls -la "$SHARED"
  cat <<'NEXT'

In the guest (open the web viewer, or RDP to the forwarded port), copy the share to a local
folder and run it:

    net use Z: \\host.lan\Data
    mkdir C:\oxrdp && copy Z:\oxagent.* C:\oxrdp\ && copy Z:\oxagent-token.txt C:\oxrdp\
    cd C:\oxrdp
    .\oxagent.exe --print-pin        # note this value; the client pins it
    .\oxagent.exe --config oxagent.conf

Then from the host:

    cargo run -p oxclient -- 127.0.0.1:AGENT_PORT --pin <pin> --token-file .vm/shared/oxagent-token.txt
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
  *) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 2 ;;
esac
