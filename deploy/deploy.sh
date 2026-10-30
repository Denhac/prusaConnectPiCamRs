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

install  Sets up a Pi with its Prusa Connect camera token and a fresh config naming the
         camera NAME (default: HOST). Use it for a new Pi, or to give a Pi a new token.
         Without --token-file you are asked for the token. A token different from the
         Pi's current one gets a new fingerprint, unless --fingerprint imports the one
         the camera is already registered with. --live offers live view in Prusa
         Connect, which needs a token from "Add web camera".
upgrade  Replaces the program and its systemd unit on Pis that are already installed.
         Their tokens, config and fingerprints are left alone.

Both build the program first, check each Pi has a working camera before copying
anything, and wait for its first upload. If an upgraded program fails, the previous
one is restored.

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
    --skip-build) build=false ;;
    --skip-camera-check) check_camera=false ;;
    -*) die "unknown option: $1 (see --help)" ;;
    *) if [[ -z $mode ]]; then mode=$1; else hosts+=("$1"); fi ;;
  esac
  shift
done

case $mode in
  install)
    (( ${#hosts[@]} == 1 )) || die "install takes exactly one host, since each camera has its own token"
    ;;
  upgrade)
    (( ${#hosts[@]} > 0 )) || die "upgrade needs at least one host"
    if [[ -n $token_file$name$fingerprint ]] || $live; then
      die "upgrade keeps each Pi's token and config; use install to change them"
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
if [[ $mode == install ]]; then
  if [[ -n $token_file ]]; then
    token=$(<"$token_file")
  elif [[ -t 0 ]]; then
    read -rsp "Prusa Connect token for ${hosts[0]}: " token
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
  local host=$1 dir=$2 files
  mkdir -p "$dir"
  cp "$binary" "$repo/packaging/prusa-connect-camera.service" "$repo/deploy/remote.sh" "$dir/"
  files=(prusa-connect-camera prusa-connect-camera.service remote.sh)
  if [[ $mode == install ]]; then
    printf '%s\n' "$token" >"$dir/token"
    write_config "${name:-$host}" "$live" <"$repo/config.example.toml" >"$dir/config.toml"
    files+=(token config.toml)
  fi
  (cd "$dir" && sha256sum -- "${files[@]}" >SHA256SUMS)
}

# Copies the example config from stdin to stdout with the camera's name set, and live view
# turned on when LIVE is true.
write_config() {
  local value=$1 live=$2 line
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  while IFS= read -r line; do
    if [[ $line == 'name = '* ]]; then
      printf 'name = "%s"\n' "$value"
    elif [[ $line == '# live = false' ]] && $live; then
      printf 'live = true\n'
    else
      printf '%s\n' "$line"
    fi
  done
}

deploy_host() {
  local host=$1 bundle=$work/bundle-$1 dir args status=0
  echo "==> $host: $mode"

  remote "$host" bash -s -- "$mode" "$check_camera" <"$repo/deploy/preflight.sh" || return 1

  make_bundle "$host" "$bundle"
  dir=$(remote "$host" mktemp -d) || return 1
  step "copying files"
  if tar -C "$bundle" -cf - . | remote "$host" tar -C "$dir" -xf -; then
    args="$mode"
    [[ -n $fingerprint ]] && args+=" --fingerprint $fingerprint"
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
failed=()
for host in "${hosts[@]}"; do
  if deploy_host "$host"; then succeeded+=("$host"); else failed+=("$host"); fi
done

echo
echo "Deployed: ${succeeded[*]:-none}"
if (( ${#failed[@]} > 0 )); then
  echo "Failed:   ${failed[*]}"
  exit 1
fi
