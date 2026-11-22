#!/usr/bin/env bash
# Run on the Pi as root by deploy.sh, from the directory it uploaded. Deletes that directory
# when done, since it may hold a token.
#   remote.sh install [--fingerprint VALUE]   the directory also holds token and config.toml
#   remote.sh upgrade
#   remote.sh add-local --id ID               the directory also holds token and output.toml
#   remote.sh remove-local --id ID
#   remote.sh rtsp                            the directory also holds rtsp.toml
#   remote.sh remove-rtsp
# Every mode also installs the program and unit from the directory. Exits 0 when every output
# uploads (or is paused), 3 when the new version runs but an output needs attention, and 1 on
# failure, after putting the previous version and settings back.
set -euo pipefail

readonly SERVICE=prusa-connect-camera
readonly BIN=/usr/local/bin/prusa-connect-camera
readonly UNIT=/etc/systemd/system/$SERVICE.service
readonly ETC=/etc/prusa-connect-camera
readonly CONFIG=$ETC/config.toml
readonly CREDENTIALS=$ETC/credentials
readonly DROP_INS=$ETC/conf.d
readonly CREDENTIAL=$CREDENTIALS/cam0
readonly FINGERPRINT=/var/lib/prusa-connect-camera/cam0.fingerprint
readonly STATUS_FILE=/run/$SERVICE/status.json
readonly RTSP_DROP_IN=$DROP_INS/rtsp.toml
# Where versions with an RTSP password kept it.
readonly OLD_RTSP_CREDENTIAL=$CREDENTIALS/rtsp
# The first upload takes about 10 s: rpicam-vid starts, then exposure settles.
readonly HEALTH_TIMEOUT=60

bundle=$(cd "$(dirname "$0")" && pwd)
trap 'rm -rf "$bundle"' EXIT
cd "$bundle"

step() { printf '  - %s\n' "$*"; }
fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

mode=${1:-}
shift || true
fingerprint=""
id=""
while (( $# )); do
  case $1 in
    --fingerprint) fingerprint=${2:?--fingerprint needs a value}; shift 2 ;;
    --id) id=${2:?--id needs a value}; shift 2 ;;
    *) fail "unknown option: $1" ;;
  esac
done
case $mode in
  add-local | remove-local) [[ $id =~ ^[A-Za-z0-9_-]+$ ]] || fail "$mode needs --id" ;;
esac
readonly drop_in=$DROP_INS/local-$id.toml output_credential=$CREDENTIALS/cam0-$id

sha256sum --quiet --strict -c SHA256SUMS || fail "files were damaged in transfer"
./prusa-connect-camera --version >/dev/null || fail "the new program doesn't run on this Pi"

# Keeps everything a deploy changes, so a failure puts all of it back together: a previous
# program may not read settings a newer one wrote.
backup=$bundle/backup
mkdir -p "$backup"
if [[ -f $BIN ]]; then cp -p "$BIN" "$backup/bin"; fi
if [[ -f $UNIT ]]; then cp -p "$UNIT" "$backup/unit"; fi
if [[ -d $ETC ]]; then cp -a "$ETC" "$backup/etc"; fi
if [[ -f $FINGERPRINT ]]; then cp -p "$FINGERPRINT" "$backup/fingerprint"; fi

# Puts the settings back as they were.
restore_settings() {
  rm -rf "$ETC"
  if [[ -d $backup/etc ]]; then cp -a "$backup/etc" "$ETC"; fi
  if [[ -f $backup/fingerprint ]]; then cp -p "$backup/fingerprint" "$FINGERPRINT"; fi
}

# Puts the previous version and its settings back, and restarts it.
restore() {
  restore_settings
  if [[ -f $backup/bin ]]; then cp -p "$backup/bin" "$BIN"; fi
  if [[ -f $backup/unit ]]; then cp -p "$backup/unit" "$UNIT"; fi
  systemctl daemon-reload
  systemctl restart "$SERVICE" || true
}

case $mode in
  install)
    install -d -m 755 "$ETC"
    install -d -m 700 "$CREDENTIALS"
    if [[ -f $CREDENTIAL ]] && cmp -s token "$CREDENTIAL"; then
      step "token unchanged; keeping the camera's fingerprint"
    else
      # A new token is a different camera in Prusa Connect, which needs a new fingerprint.
      rm -f "$FINGERPRINT"
      install -m 600 token "$CREDENTIAL"
      step "token installed"
    fi
    install -m 644 config.toml "$CONFIG"
    step "config installed"
    ;;
  upgrade | add-local | remove-local | rtsp | remove-rtsp)
    [[ -f $CONFIG ]] || fail "prusa-connect-camera isn't installed here yet; use install"
    ;;
  *) fail "usage: remote.sh install|upgrade|add-local|remove-local|rtsp|remove-rtsp [--fingerprint VALUE] [--id ID]" ;;
esac
case $mode in
  add-local)
    install -d -m 755 "$DROP_INS"
    install -m 600 token "$output_credential"
    install -m 644 output.toml "$drop_in"
    step "output $id installed"
    ;;
  remove-local)
    [[ -f $drop_in ]] || fail "there's no output $id here ($drop_in)"
    rm -f "$drop_in" "$output_credential"
    step "output $id removed"
    ;;
  rtsp)
    install -d -m 755 "$DROP_INS"
    install -m 644 rtsp.toml "$RTSP_DROP_IN"
    rm -f "$OLD_RTSP_CREDENTIAL"
    step "RTSP turned on: $(grep '^listen' "$RTSP_DROP_IN")"
    ;;
  remove-rtsp)
    [[ -f $RTSP_DROP_IN ]] || fail "RTSP isn't turned on here"
    rm -f "$RTSP_DROP_IN" "$OLD_RTSP_CREDENTIAL"
    step "RTSP turned off"
    ;;
esac

# `fingerprint show` loads and validates the config and its drop-ins, so a bad one is caught
# before the running service is touched.
# The running service still has the old settings loaded, so it needn't restart.
if ! ./prusa-connect-camera --config "$CONFIG" fingerprint show >/dev/null; then
  restore_settings
  fail "the settings are invalid; nothing was changed"
fi
if [[ -n $fingerprint ]]; then
  ./prusa-connect-camera --config "$CONFIG" fingerprint import "$fingerprint" >/dev/null
  step "fingerprint $fingerprint imported"
fi

install -m 755 prusa-connect-camera "$BIN.new"
mv -f "$BIN.new" "$BIN"
install -m 644 prusa-connect-camera.service "$UNIT"
systemctl daemon-reload
systemctl enable --quiet "$SERVICE"
step "installed build $(sha256sum "$BIN" | cut -c1-12); restarting the service"
# A failed start is caught by the health check below, which can then roll back.
systemctl restart "$SERVICE" || true

# The service's log since it last started.
service_log() {
  journalctl --no-pager -o cat _SYSTEMD_INVOCATION_ID="$(systemctl show -P InvocationID "$SERVICE")"
}

# Waits for the service to report on every output. Returns 0 when all upload or are paused,
# 2 when a server refused credentials, 3 when an output keeps failing for another reason (its
# server may be down), and 1 when the service stops or reports nothing in time.
wait_healthy() {
  local code=1 log
  for (( i = 0; i < HEALTH_TIMEOUT; i++ )); do
    sleep 1
    systemctl is-active --quiet "$SERVICE" || return 1
    if [[ -e $STATUS_FILE ]]; then
      # The installed program, which after a rollback is the previous one.
      code=0
      "$BIN" status --file "$STATUS_FILE" --quiet || code=$?
      case $code in 0 | 2) return "$code" ;; esac
    else
      # A program from before status.json, after a rollback: read its log instead.
      log=$(service_log)
      grep -q 'uploading every' <<<"$log" && return 0
      grep -q "rejected the camera's credentials" <<<"$log" && return 2
      grep -q 'pausing uploads' <<<"$log" && return 0
    fi
  done
  (( code == 3 )) && return 3
  return 1
}

show_status() {
  if [[ -e $STATUS_FILE ]]; then
    "$BIN" status --file "$STATUS_FILE" | sed 's/^/      /' || true
  fi
}

status=0
wait_healthy || status=$?
# The address the LAN reaches this Pi at: the one its route to the internet uses, which is
# never Tailscale's.
rtsp_url() {
  local ip
  ip=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
  grep -q '^listen = "127\.' "$RTSP_DROP_IN" && ip=127.0.0.1
  if [[ $ip =~ ^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\. ]]; then
    step "WARNING: this Pi reaches the internet over Tailscale ($ip); give the server the Pi's LAN address instead"
  fi
  printf 'rtsp://%s:%s/cam0' "${ip:-<this Pi>}" "$(sed -n 's/^listen = ".*:\([0-9]*\)"/\1/p' "$RTSP_DROP_IN")"
}

case $status in
  0)
    step "running:"
    show_status
    if [[ $mode == rtsp ]]; then
      step "give the prusa-connect server's camera this stream address:"
      printf '      %s\n' "$(rtsp_url)"
    fi
    exit 0
    ;;
  2)
    show_status
    fail "a server refused the credentials; the new version stays installed. Check the token, and \
that the camera isn't registered with another fingerprint"
    ;;
  3)
    step "running, but an output can't upload yet, so the new version stays installed:"
    show_status
    exit 3
    ;;
esac

echo "--- service log:"
service_log | tail -n 15
if [[ -f $backup/bin ]]; then
  step "the new version failed; putting the previous version and settings back"
  restore
  status=0
  wait_healthy || status=$?
  (( status == 0 )) && fail "the new version failed; the previous one is running again"
  fail "the new version failed, and so does the previous one"
fi
fail "the service didn't upload within ${HEALTH_TIMEOUT}s"
