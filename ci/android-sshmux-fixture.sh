#!/usr/bin/env bash
# Owned laptop-side fixtures for `ci/android.sh test <serial> sshmux`.
#
#   ci/android-sshmux-fixture.sh up <address>   start an owned sshd and owned mux servers; <address> is 127.0.0.1 or this host's own Tailscale address
#   ci/android-sshmux-fixture.sh census <name>  write the pane lists of the owned mux servers to $FIXTURE/census-<name>.json
#   ci/android-sshmux-fixture.sh down           stop every process `up` started
#
# This is a unit/protocol fixture.  A connection to this host's own
# Tailscale address is delivered through `lo`; it is never evidence of a
# private Tailscale flow.  Nothing here touches another mux server, another
# sshd, ~/.ssh, an existing key or the tailnet: every key is generated,
# every process runs with HOME and XDG_RUNTIME_DIR inside the fixture
# directory, and the only listener binds the address given to `up`.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
FIXTURE=${WEZTERM_SSHMUX_FIXTURE_DIR:-$ROOT/target/android-sshmux-fixture}
SSHD=${SSHD:-/usr/sbin/sshd}
WEZTERM=$ROOT/target/debug/wezterm
MUX_SERVER=$ROOT/target/debug/wezterm-mux-server
MISMATCH=$ROOT/target/debug/examples/sshmux_codec_mismatch
PANE_TEXT=("FIXTURE PANE ALPHA" "FIXTURE PANE BRAVO" "FIXTURE PANE CHARLIE")

die() { echo "android-sshmux-fixture.sh: $*" >&2; exit 1; }
# Every fixture process sees only the fixture's HOME and runtime directory.
isolated() { env -i PATH=/usr/bin:/bin HOME="$FIXTURE/home" XDG_RUNTIME_DIR="$FIXTURE/run" "$@"; }
cli() { local server=$1; shift; isolated WEZTERM_UNIX_SOCKET="$FIXTURE/$server.sock" "$WEZTERM" --skip-config cli --no-auto-start "$@"; }

own_address() {
  case $1 in
    127.0.0.1) echo loopback ;;
    *) tailscale ip 2>/dev/null | grep -qxF "$1" && echo self-tailscale-address-via-lo ;;
  esac
}

# await_ready <pid> <what> <log> <probe...>: neither daemon announces
# readiness, so probe it until it answers; fail as soon as it has exited.
await_ready() {
  local pid=$1 what=$2 log=$3; shift 3
  until "$@" > /dev/null 2>&1; do
    kill -0 "$pid" 2>/dev/null || die "$what exited; see $log"
  done
}

start_mux() {
  local name=$1
  cat > "$FIXTURE/$name.lua" <<LUA
return {
  unix_domains = { { name = 'unix', socket_path = '$FIXTURE/$name.sock' } },
  default_prog = { '/bin/sh', '-c', 'printf "${PANE_TEXT[0]}\\\\n"; exec cat' },
}
LUA
  env -i PATH=/usr/bin:/bin HOME="$FIXTURE/home" XDG_RUNTIME_DIR="$FIXTURE/run" \
    setsid "$MUX_SERVER" --config-file "$FIXTURE/$name.lua" > "$FIXTURE/$name.log" 2>&1 < /dev/null &
  echo $! > "$FIXTURE/$name.pid"
  local pid=$!
  await_ready "$pid" "mux server '$name'" "$FIXTURE/$name.log" cli "$name" list
}

remote_wezterm() {
  local name=$1; shift
  printf '#!/bin/sh\nexec env -i PATH=/usr/bin:/bin HOME=%q XDG_RUNTIME_DIR=%q %s\n' "$FIXTURE/home" "$FIXTURE/run" "$*" > "$FIXTURE/bin/$name"
  chmod 700 "$FIXTURE/bin/$name"
}

cmd_up() {
  local address=${1:?address} route
  route=$(own_address "$address") || true
  [ -n "$route" ] || die "'$address' is neither 127.0.0.1 nor this host's own Tailscale address; the fixture binds nothing else"
  local pidfile
  for pidfile in sshd.pid populated.pid empty.pid run/wezterm/pid; do
    [ ! -e "$FIXTURE/$pidfile" ] || die "$FIXTURE/$pidfile exists; run down first"
  done
  for bin in "$WEZTERM" "$MUX_SERVER" "$MISMATCH"; do
    [ -x "$bin" ] || die "missing $bin; run: cargo build --locked -p wezterm -p wezterm-mux-server && cargo build --locked -p wezterm-android --example sshmux_codec_mismatch"
  done
  rm -rf "$FIXTURE"
  mkdir -p -m 700 "$FIXTURE" "$FIXTURE/home" "$FIXTURE/run" "$FIXTURE/bin" "$FIXTURE/device"

  local key
  for key in host_key host_key_other client_key client_key_unauthorized; do
    ssh-keygen -q -t ed25519 -N '' -C "wezterm-fixture-$key" -f "$FIXTURE/$key"
  done
  # A trusted key of another algorithm than the Ed25519 one sshd presents.
  ssh-keygen -q -t rsa -b 3072 -N '' -C wezterm-fixture-host_key_other_type -f "$FIXTURE/host_key_other_type"
  head -c 18 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' > "$FIXTURE/passphrase"
  ssh-keygen -q -t ed25519 -N "$(cat "$FIXTURE/passphrase")" -C wezterm-fixture-client_key_encrypted -f "$FIXTURE/client_key_encrypted"
  cat "$FIXTURE/client_key.pub" "$FIXTURE/client_key_encrypted.pub" > "$FIXTURE/authorized_keys"
  chmod 600 "$FIXTURE/authorized_keys"

  start_mux populated
  cli populated spawn --new-window -- /bin/sh -c "printf '${PANE_TEXT[1]}\n'; exec cat" > /dev/null
  cli populated split-pane --pane-id 0 -- /bin/sh -c "printf '${PANE_TEXT[2]}\n'; exec cat" > /dev/null

  # An empty server: remove the pane its startup created.  If the server
  # does not survive that, there is no empty fixture; nothing replaces it.
  start_mux empty
  local empty=available
  cli empty kill-pane --pane-id 0
  if ! cli empty list --format json > "$FIXTURE/empty-after-kill.json" 2>> "$FIXTURE/empty.log" \
      || [ "$(tr -d '[:space:]' < "$FIXTURE/empty-after-kill.json")" != "[]" ]; then
    empty=blocked
  fi

  remote_wezterm wezterm-populated "WEZTERM_UNIX_SOCKET=$FIXTURE/populated.sock" "$WEZTERM" --skip-config '"$@"'
  remote_wezterm wezterm-empty "WEZTERM_UNIX_SOCKET=$FIXTURE/empty.sock" "$WEZTERM" --skip-config '"$@"'
  remote_wezterm wezterm-missing "WEZTERM_UNIX_SOCKET=$FIXTURE/missing.sock" "$WEZTERM" --skip-config '"$@"'
  remote_wezterm wezterm-mismatch "$MISMATCH"

  local port=${WEZTERM_SSHMUX_FIXTURE_PORT:-22422}
  # Password login stays offered so the client shows its secret prompt, but
  # an unprivileged sshd without PAM cannot verify any password.
  cat > "$FIXTURE/sshd_config" <<CONFIG
Port $port
ListenAddress $address
HostKey $FIXTURE/host_key
PidFile none
AuthorizedKeysFile $FIXTURE/authorized_keys
AllowUsers $USER
PubkeyAuthentication yes
PasswordAuthentication yes
KbdInteractiveAuthentication no
UsePAM no
AllowTcpForwarding no
AllowAgentForwarding no
X11Forwarding no
PermitTTY no
PermitUserEnvironment no
SetEnv WEZTERM_UNIX_SOCKET=$FIXTURE/missing.sock
LogLevel VERBOSE
CONFIG
  "$SSHD" -t -f "$FIXTURE/sshd_config"
  setsid "$SSHD" -D -e -f "$FIXTURE/sshd_config" > "$FIXTURE/sshd.log" 2>&1 < /dev/null &
  echo $! > "$FIXTURE/sshd.pid"
  await_ready "$(cat "$FIXTURE/sshd.pid")" sshd "$FIXTURE/sshd.log" grep -q "Server listening on $address port $port" "$FIXTURE/sshd.log"

  # What the instrumentation reads from the device.  Addresses and users
  # stay out of Gradle arguments and test reports.
  {
    echo "host=$address"
    echo "port=$port"
    echo "user=$USER"
    echo "route=$route"
    echo "empty=$empty"
    echo "host_fingerprint=$(ssh-keygen -l -E sha256 -f "$FIXTURE/host_key.pub" | awk '{print $2}')"
    echo "other_host_key=$(cut -d' ' -f1,2 "$FIXTURE/host_key_other.pub")"
    echo "other_type_host_key=$(cut -d' ' -f1,2 "$FIXTURE/host_key_other_type.pub")"
    echo "passphrase=$(cat "$FIXTURE/passphrase")"
    for key in populated empty missing mismatch; do echo "wezterm_$key=$FIXTURE/bin/wezterm-$key"; done
    echo "populated_panes=$(cli populated list --format json | tr -d '\n ')"
  } > "$FIXTURE/device/fixture.properties"
  cp "$FIXTURE/client_key" "$FIXTURE/device/wezterm-fixture-key"
  cp "$FIXTURE/client_key_encrypted" "$FIXTURE/device/wezterm-fixture-key-encrypted"
  cp "$FIXTURE/client_key_unauthorized" "$FIXTURE/device/wezterm-fixture-key-unauthorized"
  cp "$FIXTURE/client_key.pub" "$FIXTURE/device/wezterm-fixture-not-a-key"
  # Documents of exactly the 1 MiB key file limit and one byte more.
  local limit=$((1024 * 1024)) header='-----BEGIN OPENSSH PRIVATE KEY-----'
  { echo "$header"; head -c $((limit - ${#header} - 1)) /dev/zero | tr '\0' A; } > "$FIXTURE/device/wezterm-fixture-limit"
  { cat "$FIXTURE/device/wezterm-fixture-limit"; printf A; } > "$FIXTURE/device/wezterm-fixture-oversized"
  chmod -R go-rwx "$FIXTURE"

  {
    echo "route=$route"
    echo "empty=$empty"
    echo "server=$("$MUX_SERVER" --version)"
    echo "server_head=$(git -C "$ROOT" rev-parse HEAD)"
    echo "sshd=$("$SSHD" -V 2>&1 | head -1)"
  } | tee "$FIXTURE/fixture.env"
}

cmd_census() {
  local name=${1:?name} server
  [ -e "$FIXTURE/sshd.pid" ] || die "no fixture is up in $FIXTURE"
  {
    for server in populated empty missing; do
      if [ -S "$FIXTURE/$server.sock" ] && cli "$server" list --format json > "$FIXTURE/census.tmp" 2>/dev/null; then
        echo "{\"server\": \"$server\", \"listening\": true, \"panes\": $(cat "$FIXTURE/census.tmp")}"
      else
        echo "{\"server\": \"$server\", \"listening\": false}"
      fi
    done
  } > "$FIXTURE/census-$name.json"
  rm -f "$FIXTURE/census.tmp"
  cat "$FIXTURE/census-$name.json"
}

cmd_down() {
  local pidfile pid
  # run/wezterm/pid: the server a client without --no-auto-start (the
  # desktop default) starts for the missing-server account; its pid file
  # lock admits one per fixture.
  for pidfile in "$FIXTURE"/sshd.pid "$FIXTURE"/populated.pid "$FIXTURE"/empty.pid "$FIXTURE"/run/wezterm/pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile")
    # Match the owned executable and its exact config, not just a path substring.
    local command
    command=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || command=
    case "$command" in
      "$MUX_SERVER --config-file $FIXTURE/populated.lua "|"$MUX_SERVER --config-file $FIXTURE/empty.lua "|"$MUX_SERVER --pid-file-fd "*)
        if [ "$(tr '\0' '\n' < "/proc/$pid/environ" | grep -cxF -e "HOME=$FIXTURE/home" -e "XDG_RUNTIME_DIR=$FIXTURE/run")" = 2 ]; then
          kill "$pid"
        fi ;;
      "sshd: $SSHD -D -e -f $FIXTURE/sshd_config "*) kill "$pid" ;;
    esac
    rm -f "$pidfile"
  done
}

case "${1:-}" in
  up)     shift; cmd_up "$@" ;;
  census) shift; cmd_census "$@" ;;
  down)   shift; cmd_down "$@" ;;
  *) sed -n '2,7p' "$0"; exit 2 ;;
esac
