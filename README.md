# prusaConnectCamera

Uploads snapshots from Raspberry Pi cameras to [Prusa Connect](https://connect.prusa3d.com),
so a printer without its own camera gets one in the Connect dashboard.

It runs as one small service per Pi. It keeps `rpicam-vid` streaming at a low frame rate and
uploads the newest frame every 15 seconds. Because the camera stays running, exposure and white
balance stay settled, unlike starting `rpicam-still` for every snapshot.

**Status:** early. Runs on a Raspberry Pi 4 with a Camera Module v2 or 3 under Raspberry Pi OS
Trixie, with snapshots and [live view](#live-view) in Prusa Connect.
On a Camera Module v2 at the default 2048x1536, JPEG quality 50 and one upload every 15 seconds it uses about 5% of
one core and 70 MB of memory, and uploads about 240 KB at a time: 1.4 GB a day, 42 GB a month.
Full resolution (3280x2464) at quality 85 costs about 12% of a core and 8.6 GB a day. Most of
the CPU goes to `rpicam-vid` encoding a JPEG every second.

## Requirements

- A Raspberry Pi with a camera on its camera connector (developed for the Pi 4).
- Raspberry Pi OS Bookworm or newer, preferably 64-bit, with `rpicam-apps` installed
  (`sudo apt install rpicam-apps-lite` on Lite images). Check the camera works with
  `rpicam-hello --list-cameras`.
- A camera added in Prusa Connect: open the printer, go to **Camera**, click
  **Add new other camera** and copy its token.

## Build

Cross-compile a static binary on a Linux PC with `clang` installed, then copy
`target/aarch64-unknown-linux-musl/release/prusa-connect-camera` to the Pi:

```shell
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Or build on the Pi itself, which is slower but needs nothing else:

```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build --release
```

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

Prusa Connect can also show a live stream from the camera, with a play button over its picture.
For this the program acts as a Prusa "web camera": it speaks the same signaling and WebRTC
protocol as Prusa's browser webcam app, and streams H.264 from the Pi's hardware encoder while
someone watches. Snapshots pause during that time and resume afterwards.

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
- The viewer connects directly when it can: on the LAN, over Tailscale, or through NAT with
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
