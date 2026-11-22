#!/usr/bin/env bash
# Builds prusa-connect-camera for 64-bit Raspberry Pi OS and installs or upgrades it on
# Raspberry Pis over SSH. Run with --help for usage.
set -euo pipefail
umask 077

readonly TARGET=aarch64-unknown-linux-musl
repo=$(cd "$(dirname "$0")/.." && pwd)
readonly repo

usage() {
  cat <<'EOF'
Usage:
  deploy/deploy.sh install HOST [--token-file FILE] [--name NAME] [--fingerprint VALUE] [--live]
  deploy/deploy.sh upgrade HOST...
  deploy/deploy.sh add-local HOST --url URL --server-camera NAME [--id ID] [--token-file FILE]
                             [--insecure-http]
  deploy/deploy.sh remove-local HOST [--id ID]
  deploy/deploy.sh rtsp HOST [--listen ADDRESS:PORT]
  deploy/deploy.sh remove-rtsp HOST

install  Sets up a Pi with its Prusa Connect camera token and a fresh config naming the
         camera NAME (default: HOST), with sizes that fit its camera. Use it for a new Pi,
         or to give a Pi a new token.
         Without --token-file you are asked for the token. A token different from the
         Pi's current one gets a new fingerprint, unless --fingerprint imports the one
         the camera is already registered with. --live offers live view in Prusa
         Connect, which needs a token from "Add web camera".
upgrade  Replaces the program and its systemd unit on Pis that are already installed.
         Their tokens, config and fingerprints are left alone.
add-local
         Also sends the Pi's pictures to a self-hosted prusa-connect server at URL, as
         its camera NAME, with a token for the bot's service account (asked for unless
         given --token-file). ID (default: local) names the output; use another to add a
         second server, or the same to replace one. URL must be https, unless
         --insecure-http allows http for a test server.
remove-local
         Stops sending to the server added as ID (default: local).
rtsp     Serves the Pi's live video over RTSP at rtsp://<Pi>:8554/cam0 to anyone on a
         private network, with no password, and prints the address to give the
         prusa-connect server's camera as its stream address.
remove-rtsp
         Stops serving RTSP.

Every command builds the program first, checks each Pi has a working camera before
copying anything, installs the program, and waits for every output to upload. If the new
version fails, the previous version and settings are put back. If it runs but an output
can't upload, e.g. because its server is down, it stays and is listed as needing attention.

Options:
  --user USER          SSH user (default: rpi)
  --skip-build         Deploy the binary already built in target/
  --skip-camera-check  Deploy even if no camera is detected
  -h, --help

Passwords: set SSHPASS to log in with a password instead of an SSH key (needs sshpass).
sudo on the Pi uses PI_SUDO_PASSWORD, or SSHPASS if that is unset, or asks.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

step() { printf '  - %s\n' "$*"; }

mode=""
hosts=()
user=rpi
token_file=""
name=""
fingerprint=""
live=false
url=""
server_camera=""
output_id=local
insecure_http=false
listen=0.0.0.0:8554
build=true
check_camera=true
while (( $# )); do
  case $1 in
    -h | --help) usage; exit 0 ;;
    --user) user=${2:?--user needs a value}; shift ;;
    --token-file) token_file=${2:?--token-file needs a value}; shift ;;
    --name) name=${2:?--name needs a value}; shift ;;
    --fingerprint) fingerprint=${2:?--fingerprint needs a value}; shift ;;
    --live) live=true ;;
    --url) url=${2:?--url needs a value}; shift ;;
    --server-camera) server_camera=${2:?--server-camera needs a value}; shift ;;
    --id) output_id=${2:?--id needs a value}; shift ;;
    --insecure-http) insecure_http=true ;;
    --listen) listen=${2:?--listen needs a value}; shift ;;
    --skip-build) build=false ;;
    --skip-camera-check) check_camera=false ;;
    -*) die "unknown option: $1 (see --help)" ;;
    *) if [[ -z $mode ]]; then mode=$1; else hosts+=("$1"); fi ;;
  esac
  shift
done

case $mode in
  install | add-local | remove-local | rtsp | remove-rtsp)
    (( ${#hosts[@]} == 1 )) || die "$mode takes exactly one host, since each camera has its own token"
    ;;&
  rtsp)
    # Checked here so the TOML written below needs no escaping; the program checks again.
    [[ $listen =~ ^[0-9.]+:[0-9]+$ ]] || die "--listen must be an IPv4 address and port, e.g. 0.0.0.0:8554"
    ;;
  add-local)
    [[ -n $url && -n $server_camera ]] || die "add-local needs --url and --server-camera"
    # Checked here too so the TOML written below needs no escaping; the program checks again.
    [[ $url =~ ^(https?)://([A-Za-z0-9.-]+)(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?$ ]] ||
      die "--url must look like https://host[:port][/path]"
    scheme=${BASH_REMATCH[1]} url_host=${BASH_REMATCH[2]}
    if [[ $scheme == http ]] && ! $insecure_http; then
      die "--url is http, which sends the token unencrypted; use https, or --insecure-http for a test server"
    fi
    # Pictures go over the LAN, never over Tailscale.
    if [[ $url_host =~ ^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\. || $url_host == *.ts.net ]]; then
      die "--url is a Tailscale address; use the server's LAN name or address"
    fi
    [[ $server_camera =~ ^[A-Za-z0-9._-]+$ ]] || die "--server-camera may only contain letters, digits, '.', '-' and '_'"
    ;;&
  add-local | remove-local)
    [[ $output_id =~ ^[A-Za-z0-9_-]+$ ]] || die "--id may only contain letters, digits, '-' and '_'"
    ;;
  install | remove-rtsp) ;;
  upgrade)
    (( ${#hosts[@]} > 0 )) || die "upgrade needs at least one host"
    if [[ -n $token_file$name$fingerprint$url$server_camera ]] || $live; then
      die "upgrade keeps each Pi's tokens and settings; use install or add-local to change them"
    fi
    ;;
  *) usage >&2; exit 2 ;;
esac
if [[ -n $fingerprint && ! $fingerprint =~ ^[A-Za-z0-9-]{16,40}$ ]]; then
  die "a fingerprint is 16-40 letters, digits or '-'"
fi
if [[ -n ${SSHPASS:-} ]] && ! command -v sshpass >/dev/null; then
  die "SSHPASS is set but sshpass isn't installed"
fi

token=""
if [[ $mode == install || $mode == add-local ]]; then
  if [[ -n $token_file ]]; then
    token=$(<"$token_file")
  elif [[ -t 0 ]]; then
    if [[ $mode == install ]]; then
      read -rsp "Prusa Connect token for ${hosts[0]}: " token
    else
      read -rsp "Token of ${hosts[0]}'s service account on $url: " token
    fi
    echo
  else
    die "no token: pass --token-file, or run in a terminal to be asked for it"
  fi
  token=${token//[[:space:]]/}
  [[ $token =~ ^[[:graph:]]+$ ]] || die "the token is empty or has invalid characters"
fi

work=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/pcc-deploy.XXXXXX")
cleanup() {
  local socket
  for socket in "$work"/ssh-*; do
    [[ -S $socket ]] && ssh -o ControlPath="$socket" -O exit placeholder 2>/dev/null
  done
  rm -rf "$work"
}
trap cleanup EXIT

# One connection per Pi, shared by every command, so a password is typed at most once.
ssh_opts=(
  -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4
  -o StrictHostKeyChecking=accept-new
  -o ControlMaster=auto -o "ControlPath=$work/ssh-%C" -o ControlPersist=120
)

# Callers pass remote commands as strings built here, so expanding them locally is intended.
# shellcheck disable=SC2029
remote() {
  local host=$1
  shift
  if [[ -n ${SSHPASS:-} ]]; then
    sshpass -e ssh -o PreferredAuthentications=password,keyboard-interactive "${ssh_opts[@]}" "$user@$host" "$@"
  else
    ssh "${ssh_opts[@]}" "$user@$host" "$@"
  fi
}

sudo_password=${PI_SUDO_PASSWORD:-${SSHPASS:-}}

# Runs COMMAND as root on HOST, with whatever sudo needs.
remote_sudo() {
  local host=$1 command=$2
  if remote "$host" sudo -n true 2>/dev/null; then
    remote "$host" "sudo -n $command" </dev/null
    return
  fi
  if [[ -z $sudo_password ]]; then
    [[ -t 0 ]] || { echo "ERROR: sudo on $host needs a password; set PI_SUDO_PASSWORD" >&2; return 1; }
    read -rsp "sudo password for $user@$host: " sudo_password
    echo
  fi
  printf '%s\n' "$sudo_password" | remote "$host" "sudo -S -p '' $command"
}

if $build; then
  if command -v rustup >/dev/null && ! rustup target list --installed | grep -qx "$TARGET"; then
    die "the $TARGET target is missing; run: rustup target add $TARGET"
  fi
  echo "==> building for $TARGET"
  (cd "$repo" && cargo build --quiet --release --target "$TARGET")
fi
binary=$repo/target/$TARGET/release/prusa-connect-camera
[[ -x $binary ]] || die "no binary at $binary; run without --skip-build"

# Writes the files for HOST into DIR, with checksums so the Pi can verify the transfer.
make_bundle() {
  local host=$1 dir=$2 sensor=$3 files
  mkdir -p "$dir"
  cp "$binary" "$repo/packaging/prusa-connect-camera.service" "$repo/deploy/remote.sh" "$dir/"
  files=(prusa-connect-camera prusa-connect-camera.service remote.sh)
  if [[ $mode == install ]]; then
    printf '%s\n' "$token" >"$dir/token"
    write_config "${name:-$host}" "$live" "$sensor" <"$repo/config.example.toml" >"$dir/config.toml"
    files+=(token config.toml)
  elif [[ $mode == add-local ]]; then
    printf '%s\n' "$token" >"$dir/token"
    write_output >"$dir/output.toml"
    files+=(token output.toml)
  elif [[ $mode == rtsp ]]; then
    write_rtsp >"$dir/rtsp.toml"
    files+=(rtsp.toml)
  fi
  (cd "$dir" && sha256sum -- "${files[@]}" >SHA256SUMS)
}

# Copies the example config from stdin to stdout with the camera's name set, live view turned
# on when LIVE is true, and sizes that fit SENSOR. The config's defaults suit Camera Module v2.
write_config() {
  local value=$1 live=$2 sensor=$3 line
  local width="" height="" live_mode=""
  # Camera Module 3 is 16:9. Its 2x2-binned mode sees the whole sensor, about as many pixels as
  # the default snapshot size, while the v2's 4:3 sizes would cut off the sides.
  if [[ $sensor == imx708* ]]; then
    width=2304 height=1296 live_mode=2304:1296
  fi
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  while IFS= read -r line; do
    if [[ $line == 'name = '* ]]; then
      printf 'name = "%s"\n' "$value"
    elif [[ $line == '# live = false' ]] && $live; then
      printf 'live = true\n'
    elif [[ $line == '# width = '* && -n $width ]]; then
      printf 'width = %s\n' "$width"
    elif [[ $line == '# height = '* && -n $height ]]; then
      printf 'height = %s\n' "$height"
    elif [[ $line == '# live_mode = '* && -n $live_mode ]]; then
      printf 'live_mode = "%s"\n' "$live_mode"
    else
      printf '%s\n' "$line"
    fi
  done
}

# The drop-in for add-local's output. Its values were checked above and need no escaping.
write_output() {
  printf '%s\n' \
    "# Written by deploy/deploy.sh add-local. Its token is in the credential cam0-$output_id;" \
    "# deploy/deploy.sh remove-local --id $output_id removes both." \
    '[[output]]' \
    'type = "connect-local"' \
    "id = \"$output_id\"" \
    "url = \"$url\"" \
    "server_camera = \"$server_camera\""
  if $insecure_http; then
    printf 'insecure_http = true\n'
  fi
}

# The drop-in for rtsp. Its value was checked above and needs no escaping.
write_rtsp() {
  printf '%s\n' \
    "# Written by deploy/deploy.sh rtsp; deploy/deploy.sh remove-rtsp removes it." \
    '[rtsp]' \
    "listen = \"$listen\""
}

deploy_host() {
  local host=$1 bundle=$work/bundle-$1 dir args sensor status=0
  echo "==> $host: $mode"

  remote "$host" bash -s -- "$mode" "$check_camera" <"$repo/deploy/preflight.sh" |
    tee "$work/preflight-$host"
  (( PIPESTATUS[0] == 0 )) || return 1
  sensor=$(sed -n 's/.*, camera: \([^,]*\), service: .*/\1/p' "$work/preflight-$host")
  if [[ $mode == install && $sensor != imx219* && $sensor != imx708* ]]; then
    step "the config's sizes are tuned for Camera Module v2; check they fit this camera ($sensor)"
  fi

  make_bundle "$host" "$bundle" "$sensor"
  dir=$(remote "$host" mktemp -d) || return 1
  step "copying files"
  if tar -C "$bundle" -cf - . | remote "$host" tar -C "$dir" -xf -; then
    args="$mode"
    [[ -n $fingerprint ]] && args+=" --fingerprint $fingerprint"
    [[ $mode == *-local ]] && args+=" --id $output_id"
    remote_sudo "$host" "bash $dir/remote.sh $args" || status=$?
  else
    status=1
  fi
  # remote.sh deletes the files itself, but not if sudo or the copy failed.
  remote "$host" rm -rf "$dir" || true
  rm -rf "$bundle"
  return "$status"
}

succeeded=()
attention=()
failed=()
for host in "${hosts[@]}"; do
  status=0
  deploy_host "$host" || status=$?
  case $status in
    0) succeeded+=("$host") ;;
    3) attention+=("$host") ;;
    *) failed+=("$host") ;;
  esac
done

echo
echo "Deployed:        ${succeeded[*]:-none}"
if (( ${#attention[@]} > 0 )); then
  echo "Needs attention: ${attention[*]} (running the new version; see its outputs above)"
fi
if (( ${#failed[@]} > 0 )); then
  echo "Failed:          ${failed[*]}"
  exit 1
fi
