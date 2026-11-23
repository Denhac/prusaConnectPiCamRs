# Design: one camera hub, cloud and local outputs

Status: revision 2. It takes in the design review and the user's requirements of
2026-11-22. Branch `multi-target`.

## Progress

**Phase 0, measured on camera-pi-5 (Camera Module 3) on 2026-11-22:**
- **Encoder restart gap** (from SIGTERM to the new process's first byte; the old one exits
  in 0.05 s):

  | switch | gap |
  |---|---|
  | MJPEG → H.264 720p12 | 1.27 s |
  | H.264 720p12 → 360p10 | 1.39 s |
  | H.264 → MJPEG at 1 fps | 6.8 s, plus 3 settle frames before stills resume |

  That supports the hub's 10 s wait before switching back to stills. Camera Module v2 hasn't
  been measured.
- **Service account on the local server:** with `operator` on the camera it can push with an
  `operate`-scoped token (204).
  - A token lasts 90 days unless `expires_at` asks for up to 365.
  - A 404 carries `retryable: false` even though a restarting server also answers 404, so
    `retryable` doesn't decide anything; 404 is retried with backoff.
- **Multiple viewers, tested on xl with an experimental build:**
  - What the build changed:
    - `MAX_VIEWERS = 2`
    - only the same TURN quota's attempts are replaced
    - one encoder fanned out to every viewer, with joiners held until a keyframe
    - a test-only raw H.264 TCP tap
  - The patches are in the session scratchpad. The build is removed from xl again.
  - Results:
    - Phone (cloud), browser (cloud) and two local players shared one encoder, with up to
      3 viewers at once. No viewer was interrupted by another joining or leaving.
    - Connect never sends a Stop when a viewer closes. Sessions end through ICE failure,
      about 15 to 35 s later. Ownership per session is therefore what matters, and the
      "Stop ends only its request" change is moot, but harmless.
    - The browser's "Stream initialization error" on the first attempt also happens with the
      browser as the only viewer. Neither side gets a relay candidate while the viewer's TURN
      quota is full, and the retry about 13 s later connects. It is the known relay behaviour
      from October, not a multi-viewer problem. Holding joiners until a keyframe didn't change
      it, but stays in the hub design because it is correct.

**Phase 1, implemented and tested on xl, which sends to both Prusa Connect and its own beta
server:**
- `[[output]] type = "connect-local"` in `conf.d/*.toml` drop-ins, and `cloud = false`.
- `src/uploader.rs`, with `StillTarget` and the three error classes `Auth`, `Retry` and
  `Rejected`.
- `src/outputs/connect_local.rs`, with contract tests against a fake server.
- Credentials are read as `pcc_<name>`, through the unit's directory `LoadCredential=`.
- `status.json` and the `status` command. The cloud loop reports its state too; its log lines
  are unchanged.
- **Deploy:**
  - `add-local` and `remove-local`.
  - The health check reads the status file, with the old log markers as a fallback.
  - "Needs attention" when an output can't upload: the new version stays installed.
  - Rollback restores the binary, unit, `/etc/prusa-connect-camera` and the fingerprint
    together, and checks health with whichever binary is installed.
  - Tested on xl:
    - upgrade
    - add-local to xl's server
    - add-local to a server that is down (needs attention)
    - remove-local
    - a broken build, rolled back to the previous version with its settings
- **Differences from the plan:**
  - The unit has no transitional `cam0` line. Rollback restores the unit, so an older binary
    gets its own unit back.
  - A local-only Pi can therefore start with no `cam0` file.

**Phase 2, done and deployed to xl:**
- The cloud code lives in `src/outputs/connect_cloud/` (client, fingerprint, live).
- Its snapshots run on the shared upload loop as the `Cloud` target. The loop gained
  `printer_check` and an `on_uploading` hook, which sends `/c/info`.
- Log lines are unchanged apart from the prefix, `[cam0/cloud]`.

**Phase 3, done and deployed to xl, waiting for a live test:**
- `src/hub.rs`:
  - one encoder for all subscribers
  - caps, with the arbiter as a pure function: starts at once, restarts only lower while
    anyone watches, lingers 10 s
  - queues that survive restarts
  - joiners held until a keyframe
  - the newest keyframe kept
- rpicam's live stream is now a plain blocking channel of frames stamped with their capture
  time, consumed only by the hub.
- Cloud sessions:
  - ask with caps and time their frames by capture
  - rewrite the SPS to `42e01f` on their own copy, so other outputs get rpicam's own SPS
  - `MAX_VIEWERS = 2`
  - replace only the same request, or the same viewer's unconnected attempts
- Tested with a fake rpicam-vid script that streams made-up H.264. Two subscribers shared one
  stream, the encoder restarted only lower, and the camera went back to stills after the
  linger.
- Deferred:
  - stills on a `watch` channel, to phase 5 with the Pi API, which is what needs it
  - per-output supervision and the runtime heartbeat, to phase 4, when RTSP joins the runtime
  - a clean `rpicam-vid` stop at shutdown

The phase 3 live test on xl (2026-11-22) worked:
- The phone started one 640x360 stream. The browser (relayed, after the usual first-attempt
  relay failure) joined it with no restart.
- The camera went back to stills 11 s after the last viewer's connection dropped.

**Phase 4, done and deployed to xl:**
- `src/outputs/rtsp/`:
  - one listener on its own runtime thread, serving each camera with video at `/<id>`
  - no password and no allow list: anyone on a private network may watch (loopback, RFC 1918,
    link-local, IPv6 unique-local, and RFC 6598, which includes Tailscale), and public
    addresses are refused. This replaced Digest auth and an allow list, which the user didn't
    want on the LAN, and removed four dependencies.
  - TCP interleaved only
  - one writer per connection
  - RTSP-Info, CSeq and Session on every reply
  - incoming `$` frames discarded
  - own FU-A packetizer at 1400 bytes per packet
  - timestamps from capture time
  - a slow client drops frames until the next keyframe
  - `max_sessions`, and a status entry `rtsp`
- `[rtsp]` lives in `config.toml` or one drop-in. `deploy.sh rtsp` and `remove-rtsp`, which
  print the stream URL using the Pi's LAN address, warning if that is a Tailscale address.
- Tested:
  - **Locally, with a fake rpicam-vid running ffmpeg:**
    - ffprobe saw Constrained Baseline 1280x720
    - an unknown camera got 404
    - two ffmpeg clients ran at once from one encoder start
    - the camera returned to stills after the linger
  - **On xl, against the server's own Go client:**
    - setting `stream_url` made the camera `watch: true`
    - a WebRTC offer to the server made it connect from 10.0.0.36 and pass SETUP and PLAY
      (first with Digest, then again without auth), and the hub started 1280x720
    - when its ICE timed out, the session ended, and stills resumed 10 s later
- Still to test: a person watching through the server's UI, alongside a Prusa Connect viewer.
  That needs a printer on the local server first: its UI shows cameras on printer pages and
  dashboard cards. Until then, mpv or VLC on the RTSP address is the same multi-viewer path.

**After the first commit (5083987), on 2026-11-22:**
- **Robustness:**
  - `src/supervise.rs` puts each async part on its own runtime and thread, with a heartbeat the
    watchdog checks. Tasks that panic restart with backoff.
  - The watchdog also checks the hubs.
  - One shutdown path for SIGTERM and fatal errors: close cloud sessions (so relay allocations
    are released), then stop rpicam-vid and wait for it. No more SIGKILL at stop.
  - The hub gives frames evenly spaced times from the encoder's rate, set back to arrival time
    after half a second of drift. Arrival times alone gave equal RTP timestamps when two frames
    came in one read, which ffmpeg flagged.
- **Phase 5, the Pi API** (`src/outputs/api/`, contract in docs/api.md):
  - plain HTTP on :8555, no password, private networks only, like RTSP
  - `/v1/cameras`, `/v1/cameras/{id}`, `/v1/cameras/{id}/still?max_age&wait` (ETag = still
    number, 304, 503 with `Retry-After` while stills pause), `/v1/status`
  - `deploy.sh api` and `remove-api`
  - tested locally with the fake camera, and on xl
- **Provisioning:** `deploy/provision-local.sh` takes a Pi onto a server, logged in as an
  administrator:
  - camera, service-account grant, token
  - `add-local`, optionally `rtsp` plus the camera's `stream_url`
  - revokes older tokens of the same name once the new one uploads

  Tested twice on xl's server; the second run revoked the first run's token.

**Already there before this work:** when the service stops, the program exits without
waiting for `rpicam-vid`, and systemd SIGKILLs it (seen on 2026-11-08 as well).

## State on 2026-11-22, and the printer test

**Fleet:**
- All five Pis run build `1fa8d6ebe071` (commit 8946aeb).
- blue, green, black and orange are cloud-only.
- `connect-pi` (formerly camera-pi-5, the XL's camera) also:
  - pushes stills to the server running on itself (output `xl-test`, `http://127.0.0.1:8080`,
    server camera `camera-pi-5`, token of `service_account:camera-pi-5` until 2027-11-22)
  - serves RTSP on :8554 and the Pi API on :8555

**Server:**
- prusa-connect 0.1.0-beta.1 runs on `connect-pi` as the systemd service `prusa-connect`, as
  the upstream "Install from a release" guide lays it out:
  - user `prusa-connect`
  - binary `/var/lib/prusa-connect/bin/prusa-connect`
  - `/etc/prusa-connect/config.yaml` from the release
  - data in `/var/lib/prusa-connect/config/config.d`
  - the wizard's `setup.yaml` in `/etc/prusa-connect/config.d`
- Clients reach it at `http://connect.example.org:8080`, which resolves on the LAN
  only.
- `printer_address` is `http://connect-pi:8080`:
  - printers keep at most 20 bytes of the hostname, and the server refuses a longer one at
    startup
  - the LAN DNS (10.0.0.1) answers bare names
- It has camera `camera-pi-5` (generic, `stream_url = rtsp://10.0.0.36:8554/cam0`, no printer
  yet), and no printers.
- The old hand-run copy is in `/home/rpi/connect-pi`, as a backup.

**When a printer is on the server, test:**
1. **Move the printer.**
   - On the server's "Add a printer" page, download the settings file and check it says
     `hostname = connect-pi`, `port = 8080`, `tls = false`.
   - Load it from USB (Settings, Network, Prusa Connect, Load Settings), add the printer, and
     approve the code on the page.
   - A printer talks to one Connect server at a time, so it goes offline in Prusa Connect.
2. **Put the camera on the printer:**

   ```shell
   deploy/provision-local.sh connect-pi --camera camera-pi-5 --id xl-test \
     --server http://connect-pi:8080 --push-url http://127.0.0.1:8080 --insecure-http \
     --rtsp --printer <device id>
   ```

   - `--camera camera-pi-5` is needed because the host is no longer called that.
   - The device id is in `GET /api/v1/devices`.
   - The printer's grants are copied onto the camera the first time it is put on it.
3. **Watch in the server's UI.** On the printer's Cameras tab:
   - the still is there
   - live view plays: the server reads the Pi's RTSP and relays it over WebRTC
   - it works from a LAN-only client (a phone on the space's Wi-Fi with Tailscale off), since
     the relay also offers loopback and Tailscale candidates
4. **Several viewers at once:**
   - the server's UI, Prusa Connect's (if the camera is still viewable there with its printer
     gone), and `mpv rtsp://connect-pi:8554/cam0`
   - expect the stream to drop to the lowest quality any viewer asked for
5. **Long live view:** during a session of more than 5 minutes, the server's thumbnail goes
   blank (`stale`); check it comes back afterwards.
6. **Decide per printer** whether to keep its Prusa Connect output once it has moved. It's
   `cloud = false` in `config.toml`; ask before editing it.
7. **Other bots:** `provision-local.sh <pi> --server … --push-url
   http://connect.example.org:8080 --insecure-http --rtsp --printer …`, and
   `https` once certbot is on.

**Not done yet:**
- **certbot TLS for browsers.** The API listener then speaks only TLS 1.3, which printers can't,
  so printers need a separate connector listener (`core.connectors.<name>.config.listener`):
  plain HTTP on another port, or a private-CA ECDSA certificate naming `connect-pi`, with a
  chain of at most 1024 bytes. Then `printer_address` takes that port, and printers load the
  settings file again.
- **DHCP reservation** for 10.0.0.36, since the camera's `stream_url` uses the IP.
- **Stills from live video** (plan in "The hub").
- **The Slack module** (separate project), against docs/api.md.

## Requirements

1. **Cloud and local at once.** A Pi publishes to Prusa Connect (connect.prusa3d.com, the
   "cloud") and to one or more self-hosted prusa-connect servers ("local") at the same time.
   Each gets snapshots and live view. Neither side is abandoned.
2. **One hub per camera** owns the feed. Every use draws from it, each with several clients
   at once.
3. **At least two live clients at once**, in any mix: cloud and local, two local, or two
   cloud.
4. **A good pull API.** A future prusa-connect module (a Slack bot, a separate project) must
   be able to fetch the cameras' pictures.
5. **Stills during live view can come later.** It is acceptable that stills pause while
   someone watches. Taking a still from the live video on request must stay possible later,
   without breaking clients.
6. **A change on one side stays on that side.** The cloud protocol is undocumented, and the
   local server's API and SDK are beta. A fix for either touches only that side's module. A
   failure on one side never stops the other.
7. **Existing Pis keep working** through upgrades and rollbacks.

Decided:
- The real local install uses TLS from certbot (Let's Encrypt).
- Each bot gets its own service account on the local server.
- Slack is out of scope here.

## Facts the design builds on

Each fact gives its path, in this repo or in `~/SW/prusa-connect` (prefix `srv:`).

**This program today.**
- `source/rpicam.rs`: one `rpicam-vid`, either MJPEG at 1 fps for stills or H.264 for
  live view. Live view has one consumer: `start_live` replaces the stream and makes a new
  channel. When the process is replaced, the old receiver gets `None` and the cloud session
  ends.
- Cloud signaling:
  - `MAX_VIEWERS = 1`.
  - A `Stop` clears every session.
  - A new request ends every session that isn't yet viewing, so two people pressing play
    together collide (`live/signaling.rs`).
- `send_frames` stamps every frame at the requested fps (`live/session.rs`).
- The SPS rewrite to `42e01f` (constrained baseline, level 3.1) exists because Prusa's
  answerer rejects other profiles (`source/h264.rs`). It claims a level below what
  1280x960 needs.
- The watchdog exits the process when any thread dies (`runner.rs`). That skips the graceful
  stop that releases TURN allocations (`live/mod.rs`).
- Deploy:
  - The scripts grep log lines.
  - Rollback restores only the binary (`deploy/remote.sh`).
  - The config is `deny_unknown_fields`, so an old binary can't read a newer config.
  - `install` regenerates `config.toml` from the example.

**The local server (prusa-connect 0.1.0-beta.1).**
- **Push stills:**
  - A `generic` camera with no `snapshot_url` takes
    `PUT /api/v1/cameras/{name}/snapshot`: `image/jpeg`, at most 8 MiB, with a bearer token
    that has `operate` on the camera. It answers 204.
  - A frame is shown for `stale` (default 5 minutes) after the server received it.
  - Errors are problem documents with a `code` and `retryable`.
  - 404 covers four cases on purpose: undeclared, invisible, not a push camera, **or not
    running**. Not running includes a server restart (srv:`internal/app/cameras.go`).
- **Pull live:**
  - `stream_url = rtsp://…` makes its Go RTSP client connect: TCP interleaved only, with
    a 5 s handshake timeout for each message (srv:`adapters/camera/rtsp`).
  - It opens **one session per viewer**, only on click, and holds it until the browser's
    ICE state changes, so a forgotten tab keeps its session.
  - The WebRTC relay offers host candidates on every interface, loopback and VPNs
    included (srv:`internal/transport/webrtc/engine.go`).
- **Versioning:**
  - The REST API's compatibility gate only *reports* breaking changes before 1.0.
  - `GET /api/v1/health` needs no credential. The server version endpoint needs admin.
- **Modules:**
  - The module SDK has no camera-read method (only `camera.accept`).
  - A module holds no server credential (ADR-20260910-cqq rule 6).
  - A module can hold secret settings and declared `egress`.

## Architecture

```
                         ┌─────────────────── Hub (one per camera) ───────────────────┐
 rpicam-vid ──► source ──┤ stills: newest JPEG + seq + time, on a watch channel       │
 (one process)           │ video:  H.264 access units {data, captured, keyframe} to    │
                         │         N subscribers; subscriptions survive restarts      │
                         │ newest keyframe kept (for still-from-video later)          │
                         │ arbiter: one encoder setting from the subscribers' caps    │
                         └──────┬────────────────────────────────────┬────────────────┘
                still consumers │                                    │ video consumers
        ┌──────────────┬────────┴────────┐                  ┌────────┴─────────┐
  connect-cloud   connect-local(s)   Pi API stills      cloud WebRTC      RTSP server
  uploader        uploader(s)        (HTTPS, pull)      sessions (≤2)     sessions (≤4)
                                         ▲                                    ▲
                            Slack module, scripts, deploy         local server(s), VLC, relay
```

### What each module may know

| module | knows | must not know |
|---|---|---|
| `hub/` | frames, subscribers, caps, keyframes | Prusa, HTTP, any output |
| `source/` | rpicam-vid, MJPEG/H.264 byte streams (rpicam's own SPS, unpatched) | outputs |
| `uploader.rs` | pacing, backoff, the printer check, status reporting | any server |
| `outputs/connect_cloud/` | `/c/snapshot`, `/c/info`, fingerprints, Socket.IO, protobuf, WebRTC, TURN, Connect's presets, **the SPS rewrite** | the local server |
| `outputs/connect_local/` | `PUT …/snapshot`, bearer tokens, problem documents, `/api/v1/health` | the cloud |
| `outputs/rtsp/` | RTSP, RTP, Digest | Prusa |
| `outputs/api/` | the Pi API's HTTP contract, its TLS and tokens | Prusa |
| `status.rs` | each output's state, written to the status file | protocols |
| `config.rs` | the file format and drop-ins | protocols |

Each output exports one constructor and its config struct, and keeps everything else
private. The cloud code (`connect.rs`, `fingerprint.rs`, `live/`) moves into
`outputs/connect_cloud/`. The SPS rewrite moves there too, applied only to access units that
carry an SPS, about once a second.

## The hub

**Stills.** The newest JPEG goes out on a `tokio::sync::watch` channel as `(seq, captured,
bytes)`. Async callers await it, and the blocking uploaders use a small blocking wrapper. So
a slow still never holds a runtime worker, and no Condvar is shared with async code.

The hub has one call for every caller: `still(newer_than, max_age, wait) -> Result<Still,
Unavailable>`. `Unavailable { reason, retry_after }` replaces today's `Paused`. While video
runs, v1 returns `Unavailable { reason: "live view active" }`.

**Video.** `subscribe(caps) -> Subscription`.
- Each subscription is its own bounded queue of `Frame { data, captured: Instant, keyframe }`.
- A subscriber that falls behind drops frames until the next keyframe. A new subscriber
  starts at a keyframe, which comes within a second (`--intra` equals the fps, with inline
  SPS and PPS).
- **The hub is rpicam's only video consumer.** It attaches to each new rpicam stream, and its
  subscribers' channels stay open across encoder restarts. Lock order is hub, then rpicam
  state, never the reverse.
- Outputs take timestamps from the differences between frames' `captured` times, never from a
  requested fps. That covers WebRTC sample durations and RTSP RTP timestamps.
- The hub keeps the newest keyframe access unit, about 100 to 300 KB, replaced every second
  while video runs. Nothing uses it in v1. Still-from-video later is `decode(keyframe) ->
  JPEG`: an ffmpeg subprocess or openh264, behind one function. Then `still()` stops
  answering `Unavailable` during video, the uploaders get stills during live view for free,
  and the Pi API's 503s turn into 200s. Nothing else changes.
- The cached keyframe is never used to start new viewers sooner: the P-frames that follow it
  reference frames those viewers never got.

**Arbiter.**
- A request is a set of caps: `max_width`, `max_fps`, `max_bitrate`. The hub works out the
  dimensions from `live_mode`'s shape, so outputs never pick sizes that conflict.
- The encoder runs at the element-wise minimum of the current subscribers' caps and the
  camera's own maximum. It is a pure function, with unit tests. These rules keep restarts
  rare:
  - The first subscriber starts the encoder at once.
  - Joining with a lower cap restarts the encoder downward, about a 1 to 2 s gap for
    everyone, to be measured. Joining with an equal or higher cap changes nothing.
  - **The encoder never restarts upward while anyone is subscribed.** Leaving changes
    nothing until the last subscriber is gone.
  - Ten seconds after the last subscriber leaves, the camera returns to MJPEG stills. A
    viewer reconnecting within that time costs nothing.
- Caps per output:
  - cloud: Connect's requested preset, and SD whenever the session is relay-only
  - RTSP: configured, default 1280 wide, 12 fps, 2.5 Mbit/s

## Multiple clients

- **Cloud: `max_viewers` defaults to 2.** Signaling changes so two cloud viewers can't
  break each other:
  - A `Stop` ends only the session with its `request_id`.
  - A new request replaces only a session with the same `request_id`, or the same TURN quota
    id: the suffix of the TURN username, which the code already extracts.
  - Phase 0 checks that Prusa's cloud accepts a second viewer at all.
- **RTSP: `max_sessions` defaults to 4.**
- Cloud and RTSP sessions are separate subscribers of one hub, so any mix of two works:
  two cloud, cloud and local, or two local.
- **Bandwidth:**
  - The camera is encoded once, but each subscriber is its own copy on the Pi's Wi-Fi.
  - The local server opens one RTSP session per viewer, and keeps it while a forgotten tab
    stays open.
  - If several local viewers become common, a relay (go2rtc or MediaMTX) next to the server
    pulls one session per Pi and serves the rest. The RTSP checklist below is what lets a
    relay work without changing the Pis.

## Outputs

### Still outputs: the shared loop

```rust
pub trait StillTarget: Send {
    fn label(&self) -> &str;                   // "cloud", "local:site" → [cam0/local:site]
    fn interval(&self) -> Duration;            // validated against the target's own minimum
    fn pause_when_printer_offline(&self) -> bool; // cloud: true; local: false
    fn send(&self, still: &Still) -> Result<Sent, SendError>;
    fn on_first_success(&self) {}              // cloud: /c/info
}
pub enum SendError {
    Auth(String),      // credential refused: 5 min backoff, status "auth"
    Transient(String), // network, 5xx, local 404 (server restarting), `retryable`: exponential
    Rejected(String),  // this frame refused (413/415/400): warn, next round
}
```

- `uploader.rs` does the pacing, the printer check (only when the target asks for it),
  backoff, and status reporting.
- Each target maps its own server's answers onto these three classes, so a server that
  renumbers its errors means changing one `match`.
- `MIN_INTERVAL` (10 s) and `printer_check` belong to the cloud. Prusa Connect discards
  stills while the printer is offline, but the local server keeps them, and pausing would
  only blank its thumbnail.

### connect-cloud

The existing behaviour, moved:
- Uploads run on the shared loop.
- Sessions subscribe to the hub, with the signaling fixes above.
- The SPS rewrite is applied in the session.
- The first-success line and the credential messages keep their wording, so the scripts
  still match during the transition.

### connect-local

- **Config:**
  - `url`: https; http only with `insecure_http = true`, meant for the xl test server and
    used with a separate service account.
  - `camera`: the camera's name on that server.
  - `token` (credential), `interval` (default 15 s).
- **Validation:**
  - `url` may not contain user info, a query or a fragment.
  - `camera` is limited to `[A-Za-z0-9._-]`. It goes into a path that carries the token, so
    nothing in it may steer the request elsewhere.
  - Redirects are never followed.
- **Errors:**
  - 401 and 403 are `Auth`, logged with the problem document's `code`.
  - 404 is `Transient` with exponential backoff, and its message names all four causes.
  - Where the problem document has `retryable`, it decides.
  - Logged bodies are capped and stripped of control characters.
- **At startup** it reads `GET /api/v1/health` once (no credential) and logs what it reports.
- **The server's `stream_url`** is set by an admin. Deploy prints the exact URL, built from
  the Pi's LAN address (see Deploy).

### RTSP server

One listener per Pi (`[rtsp]`), with a path per camera: `rtsp://<pi-lan-ip>:8554/<camera
id>`. It runs on tokio. What it must do:

- **Connection:**
  - One writer task per connection, so RTP frames and RTSP replies never interleave
    mid-frame.
  - Read and discard incoming `$` frames: gortsplib and ffmpeg send RTCP over the
    connection.
  - Answer every message within the client's 5 s. DESCRIBE never waits for the encoder.
  - Close a session whose write blocks past a timeout.
- **Methods:**
  - OPTIONS: needs no auth, because keepalive OPTIONS carry `Session` but no
    `Authorization`. It refreshes the session timer.
  - DESCRIBE: H264/90000, `packetization-mode=1`, `a=control:trackID=0` in the media
    section, no Content-Base and no sprop.
  - SETUP:
    - TCP only (461 otherwise).
    - Echo `Transport: RTP/AVP/TCP;unicast;interleaved=0-1` and send
      `Session: <id>;timeout=60`.
    - Accept SETUP on the base URL and on the track URL, and PLAY on the aggregate URL.
  - PLAY replies with `Session` and `RTP-Info` (seq, rtptime).
  - TEARDOWN and GET_PARAMETER are also supported.
  - Echo `CSeq` on every reply, and route by path, ignoring the host.
- **Digest auth** (MD5, RFC 2617):
  - Exactly one challenge.
  - MD5 assumed when no `algorithm` is given.
  - `nc` isn't required to increase.
  - Nonces are time-limited HMACs, not single-use.
  - Basic is refused.
- **RTP:**
  - One SSRC per session.
  - Sequence numbers and timestamps continue across encoder restarts.
  - Timestamps come from `captured` at 90 kHz.
  - One `packetize()` per access unit, so the marker bit is on its last packet.
  - MTU of 1400 or less.
- **Access:**
  - A non-empty `allow` list is required unless the listener is loopback.
  - The password is random, at least 128 bits and URL-safe, from a credential.
- **Encryption:** RTP is cleartext on the LAN; Digest only stops unauthenticated pulls. An
  `rtsps` option (the server's client accepts it without verifying the certificate) can reuse
  the API's certificate later.

### Pi API (v1, read-only)

This is the pull API for the future module and any other client. It's a separate output with
no Prusa knowledge, built on hyper and tokio-rustls.

- **Transport:**
  - HTTPS with a self-signed certificate, generated on first start into `StateDirectory`.
    Deploy prints its SHA-256 fingerprint, and clients pin it.
  - Bearer tokens, one per client, from `api-<client>` credentials. They are compared in
    constant time, never read from query strings, and never logged.
  - The same `allow` list as RTSP.
- **`GET /v1/cameras`** returns `[{id, name, state, still: {available, seq, captured_at},
  live: {viewers}, rtsp_path}]`.
  - `state` is one of `stills`, `video`, `starting`, `failed`. The whole set is defined now.
  - `still.available` is a separate boolean. So when stills come from video later, `state`
    stays `video` and `available` becomes true, with no enum change.
- **`GET /v1/cameras/{id}/still?max_age=S&wait=T`:**
  - Answers `200 image/jpeg`, with `ETag: "<seq>"` as the main validator, plus
    `Last-Modified`. `If-None-Match` and `If-Modified-Since` answer 304.
  - `wait=T` waits up to T seconds for a frame newer than the request.
  - When nothing satisfies it, the answer is `503`, `Retry-After`, and `application/problem+json`
    `{code: "still_unavailable", reason, retryable: true}`. `reason` is free text that
    clients must not branch on.
  - The contract says a still's size may vary (stills from video use the live size), so
    still-from-video is non-breaking: 503s become 200s.
- **`GET /v1/status`** returns each output's state, from the same data as the status file.
- Live view stays on RTSP, and there is no HTTP video. A client wanting the stream gets
  `rtsp_path` and the RTSP credential.

## Config and files

- **`config.toml`** keeps today's shape, and **the cloud output stays configured in the camera
  section** (`token_file`, `live`, `interval`, `printer_check`, and the top-level
  `connect_url` and `signaling_url`).
  - A new key, `cloud = false`, turns it off for a camera.
  - Every existing config stays valid with no migration, and `install` can keep
    regenerating `config.toml`.
- **`conf.d/*.toml` drop-ins** hold everything new, read in lexical order:
  - `[[output]]` entries (`type = "connect-local"`, …), each with `camera = "<id>"` (optional
    when there is one camera)
  - `[rtsp]`
  - `[api]`

  Deploy writes and removes whole files, so bash never edits TOML, and re-running `install`
  to rotate the cloud token never touches local outputs.
- **Credentials:**
  - The unit adds `LoadCredential=pcc:/etc/prusa-connect-camera/credentials`, the directory
    form, which loads each file as `pcc_<file>`. It keeps today's `cam0` line until no
    rollback target needs it.
  - The program looks for `pcc_<name>`, then `<name>`.
  - Deploy checks `systemctl --version` (the directory form needs a recent systemd).
  - A missing path in a `LoadCredential=` line is fatal, so the `cam0` line stays only while
    the file exists.

Rejected: moving the cloud settings into an `[[output]]` and migrating existing configs. It
needs a TOML rewriter and a migration step on every Pi, with its own rollback, all to make
the format symmetric.

## Deploy, health and rollback

- **Status file.**
  - The service writes `/run/prusa-connect-camera/status.json` (`RuntimeDirectory=`)
    atomically whenever an output's state changes. States are `starting`, `ok`, `paused`,
    `auth`, `transient` and `failed`, each with a message.
  - `prusa-connect-camera status [--wait SECS]` prints it, and exits 0 once every output has
    reached a classified state.
  - `remote.sh` and `apt-upgrade.sh` switch to it. During one release the log markers stay as
    well, for older scripts.
- **Rollback.**
  - Back up the binary, `config.toml`, `conf.d/` and the unit together, and restore them
    together, followed by `daemon-reload`.
  - Before rolling back, check that the previous binary accepts the restored config.
  - Roll back only when the process crashes, or when an output reaches no classified state in
    time.
  - When a server refuses something (`auth`, or `transient` because the local server is down),
    the deploy result is **deployed, needs attention**, and nothing is rolled back.
- **New commands:**
  - `deploy.sh add-local HOST --url --camera --token-file [--insecure-http]` writes one
    drop-in and one credential.
  - `deploy.sh rtsp HOST` and `deploy.sh api HOST` turn those outputs on.
- **Never Tailscale.**
  - The RTSP URL deploy prints uses the Pi's LAN address, found on the Pi by routing toward
    the server, as `local_ip()` does today.
  - Deploy refuses a URL in 100.64.0.0/10 or fd7a:115c:a1e0::/48.
  - The program warns if a local `url` resolves into those ranges.
- **Checked on the server side:**
  - The relay's candidates include VPN and Docker addresses, so live view is tested from a
    LAN-only client.
  - A Docker install needs host networking.

## Supervision and failure isolation

- Each output runs under its own supervisor, which restarts it with backoff and shows it as
  `failed` in the status file in between.
- The process exits only when the hub, the source or the runtime fails.
- The runtime runs a heartbeat task, which the watchdog checks, so a wedged runtime is
  noticed.
- Fatal exits go through the stop path, so cloud sessions release their TURN allocations.
- Blocking work (ureq uploads, the stills wrapper) runs on its own threads, never on runtime
  workers.

## Security

- **Tokens and passwords never reach the logs.**
  - The `ureq` and `ureq_proto` log filters are both checked.
  - Problem-document bodies are capped and sanitised before logging.
  - RTSP and API credentials are never printed, except a one-time display at deploy.
- **Local tokens:**
  - Minted from a dedicated admin account. Changing that admin's password revokes every token
    they created for service accounts, which would knock out all five bots at once.
  - `docs/design/compat.md` records the rotation procedure and the one-year expiry.
- **A separate service account for the xl test server,** whose token travels over plain http.

## Tests

- **Contract tests** against a fake HTTP server, for both still targets: request shape, and
  the mapping of every status code.
- **A replayable H.264 clip** as a source, so the hub, the arbiter, cloud live and RTSP are
  tested away from a Pi.
- **An in-repo RTSP client test.** In phase 4, interop with the server's Go client on xl,
  `ffprobe`, and MediaMTX.
- **Golden tests** that load copies of the five Pis' real configs. They are copied with no
  tokens.

`docs/design/compat.md` lists, for each side, the exact surface we rely on (endpoints,
headers, statuses, protocol messages) and the version it was tested against.

## Phases

0. **Measure.**
   - The encoder restart gap, on Camera Module v2 and v3.
   - Whether Prusa's cloud accepts a second viewer (an experimental build on xl).
   - Whether a service account with `operator` on a camera can see it and push to it on xl's
     server.
1. **connect-local stills, as new code beside the untouched cloud path.**
   - The drop-ins, the credential directory, the status file and `status` command, the
     rollback changes, and `deploy.sh add-local`.
   - Result: thumbnails on the local server.
2. **The cloud uploads move onto `StillTarget`.** The log wording stays the same, so a log diff
   on xl shows no change.
3. **The hub.**
   - Fan-out, timestamps, persistent subscriptions, the arbiter, the kept keyframe, the
     signaling fixes and `max_viewers = 2`.
   - The SPS rewrite moves into the cloud module.
   - Result: two cloud viewers at once.
4. **The RTSP server** and its interop tests.
   - Result: cloud and local, or two local viewers, at once.
5. **The Pi API:** cameras, status and stills, with TLS and pinning.
6. **Later:**
   - stills from video
   - `rtsps`
   - a relay, if needed
   - ask upstream for a `camera.snapshot` SDK method

## Needed from the user

- **On xl's server**, for phase 0 and 1:
  - create a service account
  - a `generic` camera with no snapshot address, bound to the printer
  - a token with `operate` for that service account
- **For the cloud two-viewer test:** two Connect accounts, or one account in two tabs.
- **Later:** the real install's hostname.

## Alternatives considered

- **MediaMTX on each Pi as the hub.** It has mature multi-client RTSP and WebRTC, and an MJPEG
  secondary stream beside H.264. But:
  - it's a second daemon per Pi
  - its camera component tracks libcamera builds
  - config would be split across two programs
  - we'd still need an RTSP client and a depacketiser

  It stays an option for stills during live view.
- **The server pulls stills (`snapshot_url`).** The generic camera passes only a bare URL,
  which allows Basic auth in the URL and a certificate that verifies, and nothing else.
  Pushing keeps the credential on the Pi.
- **One fixed live profile, plus an SD cap for relayed viewers.** It's simpler than caps, but
  ignores a viewer who picked SD for their own bandwidth. The caps rules above keep restarts
  just as rare.
- **One process per output.** Rejected: only one process can open the camera.
