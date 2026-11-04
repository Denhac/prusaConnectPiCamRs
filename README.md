# prusaConnectCamera

Uploads snapshots from Raspberry Pi CSI cameras or Linux USB webcams to [Prusa Connect](https://connect.prusa3d.com),
so a printer without its own camera gets one in the Connect dashboard.

It runs as one small service per Pi. It keeps `rpicam-vid` (CSI) or FFmpeg (USB) streaming and
uploads the newest frame every 15 seconds. Because the camera stays running, exposure and white
balance stay settled, unlike starting `rpicam-still` for every snapshot.

**Status:** early. Runs on a Raspberry Pi 4 with a Camera Module v2 or 3 under Raspberry Pi OS
Trixie, with snapshots and [live view](#live-view) in Prusa Connect.
On a Camera Module v2 at the default 2048x1536, JPEG quality 50 and one upload every 15 seconds it uses about 5% of
one core and 70 MB of memory, and uploads about 240 KB at a time: 1.4 GB a day, 42 GB a month.
Full resolution (3280x2464) at quality 85 costs about 12% of a core and 8.6 GB a day. Most of
the CPU goes to `rpicam-vid` encoding a JPEG every second.

## Requirements

- A Raspberry Pi with a CSI camera or a Linux-compatible USB webcam (developed for the Pi 4;
  USB support and Pi 2 performance still need hardware testing).
- Raspberry Pi OS Bookworm or newer. For CSI cameras, install `rpicam-apps`
  (`sudo apt install rpicam-apps-lite` on Lite images). Check the camera works with
  `rpicam-hello --list-cameras`.
- For USB webcams, install `ffmpeg` and `v4l-utils`: `sudo apt install ffmpeg v4l-utils`.
  See [USB webcams](#usb-webcams-including-raspberry-pi-2) for configuration.
- A camera added in Prusa Connect: open the printer, go to **Camera**, click
  **Add new other camera** and copy its token.

## Build

Install Rust with rustup and the Linux build tools (`sudo apt install build-essential
pkg-config clang` on Debian/Ubuntu). The project requires Rust 1.88 or newer.

Cross-compile a static binary on a Linux PC with `clang` installed, then copy
`target/aarch64-unknown-linux-musl/release/prusa-connect-camera` to the Pi:

```shell
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Or build on the Pi itself with the build tools installed, which is slower:

```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build --release
```

For a Pi 2 running **32-bit Raspberry Pi OS**, cross-compile on an Ubuntu/Debian Linux PC
(or Ubuntu WSL):

```shell
sudo apt install gcc-arm-linux-gnueabihf libc6-dev-armhf-cross
rustup target add armv7-unknown-linux-gnueabihf
cargo build --locked --release --target armv7-unknown-linux-gnueabihf
```

Copy `target/armv7-unknown-linux-gnueabihf/release/prusa-connect-camera` to the Pi.
The verified build uses the system C library and requires glibc 2.34 or newer
(Bookworm and newer meet that requirement).

Run checks on the development machine:

```shell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# Also exercise JPEG encoding, MJPEG copying and H.264 parsing/decoding with FFmpeg:
cargo test --locked ffmpeg_synthetic_streams -- --ignored
# Verify route detection with two real WebRTC peers over loopback UDP:
cargo test --locked detects_local_video_route -- --ignored
```

Native Linux tests and both the ARMv7 and AArch64 release builds have been verified in
Ubuntu WSL with Rust 1.99.1. The FFmpeg test uses synthetic input; webcam access and Pi 2
performance still need testing on the hardware.

## Deploy over SSH

`deploy/deploy.sh` builds the program and sets it up on Pis over SSH. Before copying anything,
it checks that each Pi runs 64-bit Raspberry Pi OS and has a working camera. `install` also fits
the config to the camera: the defaults suit Camera Module v2, and a Camera Module 3 gets its 16:9
sizes.

```shell
# A new Pi, or a new token. Asks for the token unless given --token-file.
deploy/deploy.sh install rpi-bot-green
# A new program version for Pis already installed. Tokens, config and fingerprints are kept.
deploy/deploy.sh upgrade rpi-bot-blue rpi-bot-green
```

`install` takes one Pi at a time because each camera has its own token, and it replaces that
Pi's config. A new token gets a new fingerprint; `--fingerprint` imports one the camera is
already registered with. Both commands wait for each Pi's first upload, and `upgrade` restores
the previous program if the new one fails. Run `deploy/deploy.sh --help` for every option.

Without SSH keys on the Pis, set `SSHPASS` to the login password (needs `sshpass`). sudo uses
`PI_SUDO_PASSWORD`, or `SSHPASS` when that is unset.

## USB webcams (including Raspberry Pi 2)

The `usb` source uses [FFmpeg's V4L2 input](https://ffmpeg.org/ffmpeg-devices.html#video4linux2_002c-v4l2).
CSI cameras still use `type = "rpicam"`; both types can be configured in the same service.
USB capture does not require `rpicam-apps`.

Install the tools and check which capture modes your webcam supports:

```shell
sudo apt install ffmpeg v4l-utils
v4l2-ctl --list-devices
v4l2-ctl --device /dev/video0 --list-formats-ext
```

In your camera's existing config, replace its `[camera.source]` section with the following.
Choose a size, frame rate and format the webcam advertises; 640x480 at 15 fps is a starting
point, not a mode every webcam supports. `MJPG` in `v4l2-ctl` corresponds to `mjpeg` here;
`YUYV` corresponds to `yuyv422`.

```toml
[camera.source]
type = "usb"
device = "/dev/video0"
width = 640
height = 480
framerate = 15
input_format = "mjpeg"
```

Prefer a `/dev/v4l/by-id/...` device path if you have multiple cameras, since `/dev/video0`
can change after a reboot. The service already belongs to the `video` group; manual runs
also need permission to read the device. `list-cameras` remains CSI-only; use `v4l2-ctl`
for USB devices.

MJPEG snapshots copy the webcam's JPEG frames without decoding or re-encoding them. The
configured frame rate is the webcam capture rate; the camera's `interval` still controls
uploads (15 seconds by default). USB has no `quality`, rotation or CSI sensor controls in
this config. For raw-only webcams set `input_format = "yuyv422"` (or `""` for automatic
format selection); FFmpeg then encodes one JPEG per second in software.

Keep `live = false` (the default, in the `[[camera]]` section) initially on a Pi 2.
USB live view is available with `live = true` and a web-camera token, but requires FFmpeg
with `libx264` and encodes H.264 in software. Its CPU cost and usable resolution need testing
on the Pi 2. Live view scales from the configured capture mode, and snapshots pause while
someone watches, just as they do for CSI cameras.

The SSH deployment script currently requires **64-bit OS and a CSI camera**. For USB,
follow [Install by hand](#install-by-hand) using the ARMv7 build described above for a
32-bit Pi 2. The `aarch64` binary is for 64-bit systems and will not run on a 32-bit OS.
Building on the Pi 2 itself may take a long time and need swap.

Before enabling the service, check capture without a token or an upload:

```shell
prusa-connect-camera --config /etc/prusa-connect-camera/config.toml snapshot -o webcam.jpg
```

## Install by hand

These are the steps `deploy/deploy.sh install` runs for you.

```shell
sudo install -m 755 target/release/prusa-connect-camera /usr/local/bin/  # or the copied binary
sudo install -d /etc/prusa-connect-camera
sudo install -d -m 700 /etc/prusa-connect-camera/credentials
sudo install -m 644 config.example.toml /etc/prusa-connect-camera/config.toml
# Paste the token, press Enter, then Ctrl-D:
sudo sh -c 'umask 077; cat > /etc/prusa-connect-camera/credentials/cam0'
sudo install -m 644 packaging/prusa-connect-camera.service /etc/systemd/system/
sudo systemctl daemon-reload
```

Edit `/etc/prusa-connect-camera/config.toml`, then check the setup with one upload before
enabling the service:

```shell
sudo CREDENTIALS_DIRECTORY=/etc/prusa-connect-camera/credentials prusa-connect-camera test
sudo systemctl enable --now prusa-connect-camera
journalctl -u prusa-connect-camera -f
```

`test` and `snapshot` need the camera to themselves, so stop the service before running them.

Only the first successful upload is logged. For a line per upload, add
`Environment=RUST_LOG=prusa_connect_camera=debug` to the unit with `systemctl edit`. Tokens
are kept out of the logs at every level.

### Fingerprints

Prusa Connect ties a camera's token to the fingerprint sent with its first upload. Each camera's
fingerprint is generated on first use and kept in `/var/lib/prusa-connect-camera/<id>.fingerprint`.
If the camera was already registered by another uploader, import that fingerprint before the
first upload:

```shell
sudo prusa-connect-camera fingerprint import <fingerprint>
```

A camera rejected with "Camera access denied" has the wrong token or is registered with a
different fingerprint. Delete and re-add it in Prusa Connect to start over, and install within
15 minutes of adding it: Prusa Connect rejects a new camera's token if it isn't used by then.

### More than one camera

Add a `[[camera]]` section per camera (see [config.example.toml](config.example.toml)), each
with its own token from Prusa Connect, and a matching `LoadCredential=` line in the unit.

## Live view

Capture settings (`width`, `height`, `framerate`) select the USB webcam's input mode;
they do not force live-video output to use that resolution or frame rate. Live settings
arrive automatically through Prusa Connect signaling. The service waits for the established
connection before applying their scope: TURN-only settings apply to relayed video, while
local connections use the HD default (up to 1280 pixels wide at 12 fps) unless matching
settings specify otherwise. Settings scoped to all connections still apply locally.
To choose Full HD as the default, put `live_quality = "fhd"` in the `[[camera]]`
section, **before** `[camera.source]`. Available presets are `"sd"`, `"hd"` (the default),
and `"fhd"`. This changes the fallback for direct/local and STUN connections; applicable
signaling limits still take precedence, and relayed or unknown routes keep their existing
defaults. Output is capped to the capture/sensor size and keeps its aspect ratio.

For a USB webcam capable of 1080p:

```toml
[[camera]]
id = "cam0"
name = "Printer cam"
interval = 10
live = true
live_quality = "fhd"

[camera.source]
type = "usb"
device = "/dev/video0"
width = 1920
height = 1080
framerate = 30
input_format = "mjpeg"
```

Install a binary supporting `live_quality` before adding it, then restart the service and
reopen live view. With the default live frame rate this selects 1920x1080 at 12 fps and a
target bitrate of 6.5 Mbps on a local connection. The webcam's capture frame rate remains
independent. Software H.264 encoding at 1080p can be demanding on a Pi 2; check actual FPS
and CPU usage, and return to `"hd"` if it cannot keep up.

Logs show the requested quality and scope, followed by the actual connection type and
selected output resolution, frame rate and bitrate.
Route detection reads the video transport's selected ICE pair directly; missing candidate
entries in a statistics report must not cause a known local route to be treated as unknown.

Prusa Connect can also show a live stream from the camera, with a play button over its picture.
For this the program acts as a Prusa "web camera": it speaks the same signaling and WebRTC
protocol as Prusa's browser webcam app, and streams H.264 while someone watches (the Pi's
hardware encoder for CSI, FFmpeg's software encoder for USB). Snapshots pause during that
time and resume afterwards.

1. In Prusa Connect, open the printer's **Camera** tab and add a **web camera** rather than
   "Add new other camera", and copy its token.
2. Within 15 minutes, install with live view turned on:

   ```shell
   deploy/deploy.sh install rpi-bot-green --live
   ```

   Or set `live = true` in the camera's section of the config.

- One viewer at a time, as with Prusa's webcam. Connect asks for SD, HD or Full HD, and all show
  the camera's whole view, with the shape of `live_mode`: 640x480, 1280x960 and 1640x1232 on a
  Camera Module v2, 640x360, 1280x720 and 1920x1080 on a Camera Module 3.
- On a Pi 4 a stream costs about 8% of one core.
- If the camera sends no video for a minute, the session ends rather than showing a frozen
  picture, and the camera goes back to snapshots.
- The viewer connects directly when it can: on the LAN, or through NAT with
  STUN. Otherwise the stream goes through Prusa's TURN relay, which Prusa limits to SD on free
  accounts.
- The protocol is undocumented. It follows Prusa's webcam app (version 0.3.6) and may change.

## Commands

| Command | What it does |
|---|---|
| `run` | Upload from every configured camera until stopped (what the service runs) |
| `test` | Capture one frame, upload it, and print Prusa Connect's answer |
| `snapshot -o file.jpg` | Capture one frame and save it locally, e.g. to check framing |
| `list-cameras` | List the cameras and sensor modes `rpicam-vid` can see |
| `fingerprint show` / `fingerprint import <value>` | Show or set a camera's fingerprint |

All of them take `--config <path>` (default `/etc/prusa-connect-camera/config.toml`) and,
when several cameras are configured, `--camera <id>`.

## Developing without a Pi

Two settings make the full upload path work on any Linux machine:

- `token_file = "/path/to/token"` in a `[[camera]]` instead of systemd credentials.
- `source = { type = "file", path = "frame.jpg" }` to upload a fixed JPEG, or keep
  `type = "rpicam"` and point `command` at a script that writes MJPEG to stdout, such as
  `ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=1 -f mjpeg -`.

Use a separate test camera in Prusa Connect so a real camera's fingerprint isn't taken.

## TODO

- Local `/snapshot.jpg` and `/stream.mjpg`, so PrusaLink, OctoPrint or Obico can share the camera.
- Rotation by 90 degrees for cameras mounted sideways; `rpicam-vid` only rotates by 180. An EXIF
  orientation tag may be enough, if Prusa Connect's page honours it.
- An install script or `.deb`.
- An in-process camera backend using the [`libcamera`](https://crates.io/crates/libcamera) crate.
- Find out which `/c/info` settings Prusa Connect accepts besides `name`.
- Live view: TURN over TCP and TLS (webrtc-rs relays over UDP only), for viewers behind
  firewalls that block UDP; and snapshots while someone watches.

## Acknowledgements

The Prusa Connect camera API usage follows
[nvtkaszpir/prusa-connect-camera-script](https://github.com/nvtkaszpir/prusa-connect-camera-script).
No code from it is included.
