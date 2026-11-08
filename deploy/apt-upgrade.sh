#!/usr/bin/env bash
# Updates the system packages on Raspberry Pis over SSH: apt update, upgrade, autoremove and
# autoclean, on every Pi at once. Run with --help for usage.
set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: deploy/apt-upgrade.sh [--reboot] [--user USER] HOST...

Runs apt update, upgrade, autoremove and autoclean on every HOST at the same time, with
each line of output labelled with its host, then lists the Pis that need a reboot: those
whose kernel, firmware or other boot files changed.

Options:
  --reboot     Reboot the Pis that need it, then wait for them to come back and upload a
               snapshot
  --user USER  SSH user (default: rpi)
  -h, --help

Passwords: set SSHPASS to log in with a password instead of an SSH key (needs sshpass).
sudo on the Pi uses PI_SUDO_PASSWORD, or SSHPASS if that is unset, or asks once for all Pis.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

hosts=()
user=rpi
reboot=no-reboot
while (( $# )); do
  case $1 in
    -h | --help) usage; exit 0 ;;
    --reboot) reboot=reboot ;;
    --user) user=${2:?--user needs a value}; shift ;;
    -*) die "unknown option: $1 (see --help)" ;;
    *) hosts+=("$1") ;;
  esac
  shift
done
(( ${#hosts[@]} > 0 )) || { usage >&2; exit 2; }
if [[ -n ${SSHPASS:-} ]] && ! command -v sshpass >/dev/null; then
  die "SSHPASS is set but sshpass isn't installed"
fi

work=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/pcc-apt.XXXXXX")
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

# Runs on each Pi as root, inside single quotes, so it must not contain any. apt-get is apt's
# command for scripts; --with-new-pkgs makes its upgrade install new dependencies, such as a
# new kernel, as apt upgrade does. Changed config files keep the local version, rather than
# stopping to ask. Raspberry Pi OS doesn't create /run/reboot-required for a new kernel or
# firmware, so changes to the boot files since boot count too. With "reboot" as $1, a Pi that
# needs it reboots a few seconds later, after the SSH session has ended cleanly.
read -r -d '' upgrade <<'EOF' || true
set -e
export DEBIAN_FRONTEND=noninteractive
apt=(apt-get -q -o DPkg::Lock::Timeout=600)
"${apt[@]}" update
"${apt[@]}" -y --with-new-pkgs -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold upgrade
"${apt[@]}" -y autoremove
"${apt[@]}" -y autoclean
booted=$(uptime -s)
if [ -e /run/reboot-required ] ||
   [ -n "$(find /boot /boot/firmware -maxdepth 1 -newermt "$booted" -print -quit 2>/dev/null)" ]; then
  echo "reboot needed: boot files changed since $booted"
  if [ "$1" = reboot ]; then
    systemd-run --quiet --on-active=5 systemctl reboot
    echo "rebooting in 5 seconds"
  fi
fi
EOF

# Connect to each Pi in turn first, so any password prompts come one at a time. The boot ID
# tells when a Pi has rebooted.
declare -A passwordless_sudo=() boot_id=()
connected=()
failed=()
for host in "${hosts[@]}"; do
  if ! boot_id[$host]=$(remote "$host" cat /proc/sys/kernel/random/boot_id </dev/null); then
    echo "==> $host: can't connect" >&2
    failed+=("$host")
    continue
  fi
  connected+=("$host")
  # Raspberry Pi OS shares a sudo login across sessions for 15 minutes, and a reboot ends it.
  # -k ignores it, so a recent sudo isn't mistaken for passwordless sudo.
  if remote "$host" sudo -n -k true </dev/null 2>/dev/null; then
    passwordless_sudo[$host]=1
  fi
done

sudo_password=${PI_SUDO_PASSWORD:-${SSHPASS:-}}
if [[ -z $sudo_password ]] && (( ${#passwordless_sudo[@]} < ${#connected[@]} )); then
  [[ -t 0 ]] || die "sudo needs a password; set PI_SUDO_PASSWORD"
  read -rsp "sudo password on the Pis: " sudo_password
  echo
fi

# Runs COMMAND as root on HOST. -k makes sudo read the password even while a recent login
# means it isn't needed, so the password never reaches COMMAND's stdin.
remote_root() {
  local host=$1 command=$2
  if [[ -n ${passwordless_sudo[$host]:-} ]]; then
    remote "$host" "sudo -n $command" </dev/null
  else
    printf '%s\n' "$sudo_password" | remote "$host" "sudo -k -S -p '' $command"
  fi
}

# Upgrades HOST, then writes the exit status to $work/HOST.status.
upgrade_host() {
  local host=$1 status=0
  remote_root "$host" "bash -c '$upgrade' apt-upgrade $reboot" || status=$?
  echo "$status" >"$work/$host.status"
}

# Waits up to 5 minutes for HOST to come back from its reboot and upload a snapshot, then
# writes 0 to $work/HOST.back if both happened. The service logs its first upload, or that it
# is pausing uploads while the printer is unreachable.
await_reboot() {
  local host=$1 deadline=$((SECONDS + 300)) id="" log=""
  # The connection to the old boot closes as the Pi shuts down; don't wait for it.
  ssh "${ssh_opts[@]}" -O exit "$user@$host" 2>/dev/null || true
  echo 1 >"$work/$host.back"
  sleep 20
  until [[ -n $id && $id != "${boot_id[$host]}" ]]; do
    if (( SECONDS > deadline )); then
      echo "not back after 5 minutes"
      return
    fi
    sleep 5
    id=$(remote "$host" cat /proc/sys/kernel/random/boot_id </dev/null 2>/dev/null) || id=""
  done
  echo "back up, running kernel $(remote "$host" uname -r </dev/null)"
  until grep -qE 'uploading every|pausing uploads' <<<"$log"; do
    if (( SECONDS > deadline )); then
      echo "camera service: $(remote "$host" systemctl is-active prusa-connect-camera </dev/null), no upload since the reboot"
      return
    fi
    sleep 5
    log=$(remote_root "$host" "journalctl -b -u prusa-connect-camera -o cat --no-pager" 2>/dev/null) || log=""
  done
  echo "camera service: $(grep -m1 -oE '(uploading every|pausing uploads).*' <<<"$log")"
  echo 0 >"$work/$host.back"
}

# Prefixes each line from stdin with LABEL.
label() {
  local line
  while IFS= read -r line || [[ -n $line ]]; do
    printf '[%s] %s\n' "$1" "$line"
  done
}

for host in "${connected[@]}"; do
  upgrade_host "$host" 2>&1 | tee "$work/$host.log" | label "$host" &
done
wait

updated=()
needs_reboot=()
for host in "${connected[@]}"; do
  if [[ $(<"$work/$host.status") != 0 ]]; then
    failed+=("$host")
    # The output is interleaved with the other Pis', so repeat the end of it.
    printf '\n--- %s failed; its last lines:\n' "$host"
    tail -n 15 "$work/$host.log"
    continue
  fi
  updated+=("$host")
  if grep -q '^reboot needed:' "$work/$host.log"; then
    needs_reboot+=("$host")
  fi
done

rebooted=()
if [[ $reboot == reboot ]] && (( ${#needs_reboot[@]} > 0 )); then
  echo
  for host in "${needs_reboot[@]}"; do
    await_reboot "$host" 2>&1 | label "$host" &
  done
  wait
  for host in "${needs_reboot[@]}"; do
    if [[ $(<"$work/$host.back") == 0 ]]; then rebooted+=("$host"); else failed+=("$host"); fi
  done
fi

echo
echo "Updated:       ${updated[*]:-none}"
if [[ $reboot == reboot ]]; then
  echo "Rebooted:      ${rebooted[*]:-none}"
elif (( ${#needs_reboot[@]} > 0 )); then
  echo "Reboot needed: ${needs_reboot[*]}"
fi
if (( ${#failed[@]} > 0 )); then
  echo "Failed:        ${failed[*]}"
  exit 1
fi
