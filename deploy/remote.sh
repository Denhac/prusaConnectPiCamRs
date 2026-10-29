#!/usr/bin/env bash
# Run on the Pi as root by deploy.sh, from the directory it uploaded. Deletes that directory
# when done, since it may hold a token.
#   remote.sh install [--fingerprint VALUE]   the directory also holds token and config.toml
#   remote.sh upgrade
set -euo pipefail

readonly SERVICE=prusa-connect-camera
readonly BIN=/usr/local/bin/prusa-connect-camera
readonly ETC=/etc/prusa-connect-camera
readonly CONFIG=$ETC/config.toml
readonly CREDENTIAL=$ETC/credentials/cam0
readonly FINGERPRINT=/var/lib/prusa-connect-camera/cam0.fingerprint
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
while (( $# )); do
  case $1 in
    --fingerprint) fingerprint=${2:?--fingerprint needs a value}; shift 2 ;;
    *) fail "unknown option: $1" ;;
  esac
done

sha256sum --quiet --strict -c SHA256SUMS || fail "files were damaged in transfer"
./prusa-connect-camera --version >/dev/null || fail "the new program doesn't run on this Pi"

case $mode in
  install)
    install -d -m 755 "$ETC"
    install -d -m 700 "$ETC/credentials"
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
  upgrade)
    [[ -s $CREDENTIAL && -f $CONFIG ]] || fail "prusa-connect-camera isn't installed here yet; use install"
    ;;
  *) fail "usage: remote.sh install|upgrade [--fingerprint VALUE]" ;;
esac

# `fingerprint show` loads and validates the config, so a bad one is caught before the
# running service is touched.
./prusa-connect-camera --config "$CONFIG" fingerprint show >/dev/null || fail "$CONFIG is invalid"
if [[ -n $fingerprint ]]; then
  ./prusa-connect-camera --config "$CONFIG" fingerprint import "$fingerprint" >/dev/null
  step "fingerprint $fingerprint imported"
fi

# Keep the running program for rollback, unless it is this same build.
previous=""
if [[ -f $BIN ]] && ! cmp -s prusa-connect-camera "$BIN"; then
  cp -p "$BIN" "$BIN.prev"
  previous=$BIN.prev
fi
install -m 755 prusa-connect-camera "$BIN.new"
mv -f "$BIN.new" "$BIN"
install -m 644 prusa-connect-camera.service "/etc/systemd/system/$SERVICE.service"
systemctl daemon-reload
systemctl enable --quiet "$SERVICE"
step "installed build $(sha256sum "$BIN" | cut -c1-12); restarting the service"
# A failed start is caught by the health check below, which can then roll back.
systemctl restart "$SERVICE" || true

# The service's log since it last started.
service_log() {
  journalctl --no-pager -o cat _SYSTEMD_INVOCATION_ID="$(systemctl show -P InvocationID "$SERVICE")"
}

# Waits for the first upload. Returns 2 if Prusa Connect rejected the credentials and 3 if
# uploads are paused because the printer is unreachable.
wait_for_upload() {
  local log
  for (( i = 0; i < HEALTH_TIMEOUT; i++ )); do
    sleep 1
    log=$(service_log)
    grep -q 'uploading every' <<<"$log" && return 0
    grep -q "rejected the camera's credentials" <<<"$log" && return 2
    grep -q 'pausing uploads' <<<"$log" && return 3
    systemctl is-active --quiet "$SERVICE" || return 1
  done
  return 1
}

status=0
wait_for_upload || status=$?
case $status in
  0) step "uploading to Prusa Connect"; exit 0 ;;
  3) step "running, but the printer is unreachable, so uploads are paused"; exit 0 ;;
esac

echo "--- service log:"
service_log | tail -n 15
if (( status == 2 )); then
  fail "Prusa Connect rejected the token; check it, and that this camera isn't registered with another fingerprint"
fi
if [[ -n $previous ]]; then
  step "the new program failed; rolling back"
  mv -f "$previous" "$BIN"
  systemctl restart "$SERVICE" || true
  status=0
  wait_for_upload || status=$?
  (( status == 0 )) && fail "the new program failed; the previous one is running again"
  fail "the new program failed, and so does the previous one"
fi
fail "the service didn't upload within ${HEALTH_TIMEOUT}s"
