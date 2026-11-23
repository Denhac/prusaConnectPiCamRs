#!/usr/bin/env bash
# Sets a Pi up on a self-hosted prusa-connect server: the camera, the bot's service account
# and its token on the server, then the output (and optionally RTSP) on the Pi. Run with --help
# for usage.
set -euo pipefail
umask 077

repo=$(cd "$(dirname "$0")/.." && pwd)
readonly repo

usage() {
  cat <<'EOF'
Usage: deploy/provision-local.sh HOST --server URL [options]

Sets up the Pi HOST on a self-hosted prusa-connect server, as an administrator:
  1. adds (or updates) a generic camera NAME on the server, on PRINTER if given
  2. gives the bot's service account (service_account:NAME) the operator role on it
  3. mints a token for that account with the operate scope
  4. runs deploy.sh add-local, so the Pi sends its pictures with that token
  5. with --rtsp, runs deploy.sh rtsp and sets the camera's stream address to the Pi's
  6. revokes the bot's older tokens of the same name, once the new one works
Running it again replaces the token and keeps everything else.

Options:
  --server URL       The server as this computer reaches it, for the admin API.
  --push-url URL     The server as the Pi reaches it (default: --server). Use its LAN address.
  --camera NAME      The camera's name on the server (default: HOST). Lower-case letters,
                     digits and '-'.
  --printer ID       The device id of the printer the camera watches.
  --id ID            The output's id on the Pi (default: local).
  --days N           How long the token lasts, at most 365 (default: 365).
  --rtsp             Also serve live video from the Pi and use it for the camera's live view.
  --insecure-http    Allow http:// addresses, which send passwords and tokens unencrypted.
                     For a test server only.
  -h, --help

The administrator's login comes from PC_ADMIN_USER and PC_ADMIN_PASSWORD, or is asked for.
SSHPASS and PI_SUDO_PASSWORD work as for deploy.sh.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

step() { printf '  - %s\n' "$*"; }

host=""
server=""
push_url=""
camera=""
printer=""
output_id=local
days=365
rtsp=false
insecure_http=false
while (( $# )); do
  case $1 in
    -h | --help) usage; exit 0 ;;
    --server) server=${2:?--server needs a value}; shift ;;
    --push-url) push_url=${2:?--push-url needs a value}; shift ;;
    --camera) camera=${2:?--camera needs a value}; shift ;;
    --printer) printer=${2:?--printer needs a value}; shift ;;
    --id) output_id=${2:?--id needs a value}; shift ;;
    --days) days=${2:?--days needs a value}; shift ;;
    --rtsp) rtsp=true ;;
    --insecure-http) insecure_http=true ;;
    -*) die "unknown option: $1 (see --help)" ;;
    *) [[ -z $host ]] || die "one host at a time"; host=$1 ;;
  esac
  shift
done
[[ -n $host && -n $server ]] || { usage >&2; exit 2; }
server=${server%/}
push_url=${push_url:-$server}
camera=${camera:-$host}
[[ $camera =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] ||
  die "--camera must be lower-case letters, digits and '-' (a DNS label), e.g. camera-pi-5"
[[ -z $printer || $printer =~ ^[A-Za-z0-9-]+$ ]] || die "--printer is a device id: letters, digits and '-'"
if ! [[ $days =~ ^[0-9]+$ ]] || (( days < 1 || days > 365 )); then
  die "--days must be 1 to 365"
fi
for url in "$server" "$push_url"; do
  if [[ $url == http://* ]] && ! $insecure_http; then
    die "$url is http, which sends the login and token unencrypted; use https, or --insecure-http for a test server"
  fi
done
command -v curl >/dev/null || die "needs curl"
command -v python3 >/dev/null || die "needs python3"

user=${PC_ADMIN_USER:-}
password=${PC_ADMIN_PASSWORD:-}
if [[ -z $user || -z $password ]]; then
  [[ -t 0 ]] || die "set PC_ADMIN_USER and PC_ADMIN_PASSWORD, or run in a terminal"
  [[ -n $user ]] || read -rp "Administrator on $server: " user
  [[ -n $password ]] || { read -rsp "Password for $user: " password; echo; }
fi

work=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/pcc-provision.XXXXXX")
logged_in=false
cleanup() {
  if $logged_in; then
    curl -sS -m 10 -b "$work/cookies" -X POST "$server/api/v1/auth/logout" -o /dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# Calls the server's API as the administrator: api METHOD PATH [JSON] [HEADER...]. The answer's
# body goes to $work/body, and its status is printed.
api() {
  local method=$1 path=$2 body=${3:-} args=()
  shift $(( $# < 3 ? $# : 3 ))
  for header in "$@"; do args+=(-H "$header"); done
  if [[ -n $body ]]; then
    printf '%s' "$body" >"$work/request"
    args+=(-H 'Content-Type: application/json' --data-binary "@$work/request")
  fi
  curl -sS -m 30 -b "$work/cookies" -c "$work/cookies" -X "$method" "${args[@]}" \
    -o "$work/body" -w '%{http_code}' "$server$path"
}

# Prints the problem document the last call answered, without anything it might echo back.
problem() {
  python3 -I -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
    print(" ".join(str(d.get(k, "")) for k in ("code", "title", "detail")).strip())
except Exception:
    print(open(sys.argv[1], errors="replace").read(300).strip())
' "$work/body"
}

# Reads FIELD from the JSON in $work/body.
field() {
  python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$work/body" "$1"
}

echo "==> $host on $server, as camera $camera"

credentials=$(USER_NAME=$user PASSWORD=$password python3 -I -c '
import json, os
print(json.dumps({"username": os.environ["USER_NAME"], "password": os.environ["PASSWORD"]}))')
status=$(api POST /api/v1/auth/login "$credentials")
unset credentials password
[[ $status == 200 ]] || die "logging in failed (HTTP $status): $(problem)"
logged_in=true
step "logged in as $user"

# Adds the camera, or changes the one there, keeping its other settings. STREAM_URL, when
# given, becomes its stream address.
put_camera() {
  local stream_url=${1:-} settings version="" body status
  status=$(api GET /api/v1/camera-settings)
  [[ $status == 200 ]] || die "reading the cameras failed (HTTP $status): $(problem)"
  settings=$(CAMERA=$camera python3 -I -c '
import json, os, sys
items = json.load(open(sys.argv[1]))["items"]
mine = [c for c in items if c["name"] == os.environ["CAMERA"]]
print(json.dumps(mine[0]) if mine else "")' "$work/body")
  if [[ -n $settings ]]; then
    version=$(python3 -I -c 'import json, sys; print(json.loads(sys.argv[1])["version"])' "$settings")
  fi
  body=$(SETTINGS=$settings PRINTER=$printer STREAM_URL=$stream_url python3 -I -c '
import json, os
settings = json.loads(os.environ["SETTINGS"] or "{}")
if settings and settings.get("type") != "generic":
    raise SystemExit("the camera exists with type " + settings["type"] + ", not generic")
config = settings.get("config", {})
config.pop("snapshot_url", None)
if os.environ["PRINTER"]:
    config["printer"] = os.environ["PRINTER"]
if os.environ["STREAM_URL"]:
    config["stream_url"] = os.environ["STREAM_URL"]
print(json.dumps({"type": "generic", "config": config}))')
  if [[ -n $version ]]; then
    status=$(api PUT "/api/v1/cameras/$camera" "$body" "If-Match: \"$version\"")
  else
    status=$(api PUT "/api/v1/cameras/$camera" "$body" "If-None-Match: *")
  fi
  [[ $status == 200 || $status == 201 ]] || die "saving camera $camera failed (HTTP $status): $(problem)"
}

put_camera
step "camera $camera is a generic camera${printer:+ on printer $printer}, taking pushed stills"

subject=service_account:$camera
grant=$(SUBJECT=$subject python3 -I -c '
import json, os; print(json.dumps({"relation": "operator", "subject": os.environ["SUBJECT"]}))')
status=$(api POST "/api/v1/grants/device/$camera" "$grant")
[[ $status == 200 || $status == 201 ]] || die "granting $subject operator failed (HTTP $status): $(problem)"
step "$subject is an operator of camera $camera"

token_name="$camera camera (prusa-connect-camera)"
mint=$(NAME=$token_name SUBJECT=$subject DAYS=$days python3 -I -c '
import datetime, json, os
expires = datetime.datetime.now(datetime.UTC) + datetime.timedelta(days=int(os.environ["DAYS"]), minutes=-5)
print(json.dumps({"name": os.environ["NAME"], "subject": os.environ["SUBJECT"], "scopes": ["operate"],
                  "expires_at": expires.strftime("%Y-%m-%dT%H:%M:%SZ")}))')
status=$(api POST /api/v1/auth/credentials "$mint")
[[ $status == 201 ]] || die "minting a token failed (HTTP $status): $(problem)"
# The answer holds the token: keep it in a file of its own, and never print it.
python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1]))
open(sys.argv[2], "w").write(d["token"] + "\n")
print(d["id"], d["expires_at"])' "$work/body" "$work/token" >"$work/minted"
rm -f "$work/body"
read -r token_id expires_at <"$work/minted"
step "minted a token for $subject, valid until $expires_at"

add_local=(add-local "$host" --url "$push_url" --server-camera "$camera" --id "$output_id"
  --token-file "$work/token")
$insecure_http && add_local+=(--insecure-http)
"$repo/deploy/deploy.sh" "${add_local[@]}" || die "deploy.sh add-local failed; the new token is left unused"

if $rtsp; then
  "$repo/deploy/deploy.sh" rtsp "$host" --skip-build | tee "$work/rtsp" || die "deploy.sh rtsp failed"
  stream_url=$(grep -oE 'rtsp://[0-9.]+:[0-9]+/[A-Za-z0-9_-]+' "$work/rtsp" | tail -n 1)
  [[ -n $stream_url ]] || die "deploy.sh rtsp printed no stream address"
  put_camera "$stream_url"
  step "camera $camera plays live view from $stream_url"
fi

status=$(api GET "/api/v1/auth/credentials?subject=$subject")
[[ $status == 200 ]] || die "listing $subject's tokens failed (HTTP $status): $(problem)"
old=$(NAME=$token_name KEEP=$token_id python3 -I -c '
import json, os, sys
for c in json.load(open(sys.argv[1]))["items"]:
    if c["name"] == os.environ["NAME"] and c["id"] != os.environ["KEEP"]:
        print(c["id"])' "$work/body")
for id in $old; do
  status=$(api DELETE "/api/v1/auth/credentials/$id")
  [[ $status == 204 || $status == 200 ]] || step "couldn't revoke old token $id (HTTP $status): $(problem)"
done
[[ -n $old ]] && step "revoked $(wc -w <<<"$old") older token(s) of $subject"
echo "==> $host is set up on $server"
