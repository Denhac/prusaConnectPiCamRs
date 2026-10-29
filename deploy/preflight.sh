#!/usr/bin/env bash
# Run on the Pi as the SSH user by deploy.sh, before anything is copied. Changes nothing.
#   preflight.sh install|upgrade CHECK_CAMERA
set -uo pipefail

mode=$1
check_camera=$2
problems=()

model=$(tr -d '\0' </proc/device-tree/model 2>/dev/null || echo "unknown model")
# shellcheck source=/dev/null
. /etc/os-release
camera="not checked"

[[ $(uname -m) == aarch64 ]] || problems+=("needs 64-bit Raspberry Pi OS; this is $(uname -m)")
command -v systemctl >/dev/null || problems+=("systemd isn't available")
if ! command -v rpicam-vid >/dev/null; then
  problems+=("rpicam-vid is missing: sudo apt install rpicam-apps-lite")
elif [[ $check_camera == true ]]; then
  # Listing works even while the service is using the camera.
  camera=$(timeout 20 rpicam-vid --list-cameras 2>/dev/null | sed -n 's/^ *0 : \([^ ]*\).*/\1/p')
  [[ -n $camera ]] || problems+=("no camera detected: check the ribbon cable (contacts facing the HDMI ports), then reboot, since cameras are only detected at boot")
fi
if [[ $mode == upgrade && ! -f /etc/prusa-connect-camera/config.toml ]]; then
  problems+=("prusa-connect-camera isn't installed here yet; use install")
fi

echo "  - $model, $PRETTY_NAME, camera: ${camera:-none}, service: $(systemctl is-active prusa-connect-camera 2>/dev/null)"
for problem in "${problems[@]}"; do
  echo "ERROR: $problem" >&2
done
(( ${#problems[@]} == 0 ))
