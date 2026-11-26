# What we rely on from each server

When an upload or live view breaks after a server changes, this lists what this program
depends on, so the change can be found quickly. Each side's code lives in one place, shown
below. Update this file whenever that code starts relying on something new.

## Prusa Connect (connect.prusa3d.com)

The code is in `src/outputs/connect_cloud/`. None of this is a documented API. It follows
nvtkaszpir/prusa-connect-camera-script and Prusa's browser webcam app, version 0.3.6.

| what | details |
|---|---|
| `PUT /c/snapshot` | headers `Token` and `Fingerprint`; body `image/jpg`. 200 as of October 2026 (previously 204). 401/403 mean a wrong token or a different fingerprint. |
| `PUT /c/info` | `{"config":{"name":…}}`; with live view also `"type":"WEB"` and `"features":["SocketCom","WebRtc"]`. The 200 response echoes the token, so it is never read or logged. |
| Fingerprint | 16–40 letters, digits or `-`. Bound to the token on the first upload. A new token needs a new fingerprint. |
| Token lifetime | A new camera's token must be used within about 15 minutes, or it's rejected. |
| Signaling | Socket.IO at camera-signaling.prusa3d.com, origin `https://camera-service-webcam.prusa3d.com`, protobuf messages (`live/proto.rs`), reauthentication every 12 h. |
| WebRTC | The camera offers. H.264 `profile-level-id=42e01f` (the SPS is rewritten to match), a `ping-pong` data channel, ICE trickled through signaling, and TURN credentials in the request. |
| TURN quota | Per viewer. 486 means the allocation quota was reached; a session must release its allocation on close. |

## Self-hosted prusa-connect

The code is in `src/outputs/connect_local.rs`. Tested with **0.1.0-beta.1**. Before 1.0 the
server's API compatibility gate only reports breaking changes; it doesn't block them.

| what | details |
|---|---|
| `PUT /api/v1/cameras/{name}/snapshot` | `Authorization: Bearer pcl_…`, `Content-Type: image/jpeg`, at most 8 MiB. 204 on success. Needs `operate` on the camera. |
| Camera | `type: generic` with no `snapshot_url`. A pushed frame is shown for `stale` (default 5m) after the server received it. |
| Errors | `application/problem+json` with `code`, `title`, `detail`, `retryable`. Seen on 0.1.0-beta.1: 401 `core.auth/TOKEN_INVALID`, 404 `core.validation/RESOURCE_NOT_FOUND` (`retryable: false`), 415 `core.validation/FRAME_NOT_AN_IMAGE`. |
| 404 | One answer for four causes: the camera is undeclared, can't be seen by the token's account, doesn't take pushed frames, or isn't running (a server restart). So it is retried. |
| `GET /api/v1/health` | No credential. `{"status":"ok","version":"0.1.0-beta.1",…}`. Used only to log the version. |
| Tokens | Minted with `POST /api/v1/auth/credentials` for `service_account:<name>`, scope `operate`. 90 days by default, a year at most. Revoked when the minting admin's password changes. |
| Grants | `POST /api/v1/grants/device/<camera>` with `{"relation":"operator","subject":"service_account:<name>"}`. A camera's device id is its name. |

### Live view: the server's RTSP client

The code is in `src/outputs/rtsp/`. The server's client is in `adapters/camera/rtsp/client.go`
in the server's source. Tested against 0.1.0-beta.1 on xl, and against ffmpeg and ffprobe.

| what | details |
|---|---|
| Methods | OPTIONS, then DESCRIBE, SETUP, PLAY; OPTIONS with `Session` every timeout/2 as keepalive; TEARDOWN at the end. |
| Transport | TCP interleaved only (`RTP/AVP/TCP;unicast;interleaved=0-1`). It ignores RTCP. |
| Auth | It sends each request without credentials first, and answers a 401 with Basic or Digest (MD5, `qop=auth`, `nc=00000001`). We ask for none: any client on a private network may watch, and public addresses are refused. |
| URLs | Base = the URL without user info or a trailing `/`. SETUP goes to base + `/` + the media section's `a=control` (`trackID=0`); PLAY to base. Content-Base would replace the base; we send none. |
| Timeouts | 5 s per handshake message. The keepalive interval comes from `Session: id;timeout=N` (we say 60), and falls back to the 5 s handshake timeout, i.e. every 2.5 s. |
| SDP | It reads `m=video`, `a=rtpmap` (must be H264), `a=control`, and the optional `sprop-parameter-sets` (we send none; SPS and PPS come inline). |
| Sessions | One RTSP session per viewer, opened when a viewer presses play and closed when its WebRTC connection ends (30 s after an ICE failure). |
| Camera | `generic` with `stream_url` set and no `snapshot_url`: it still takes pushed stills, and lists formats `jpeg` and `h264/annexb` with `watch: true`. |
| Relay | Its WebRTC answer offers host candidates on every interface: loopback, LAN, and Tailscale v4 and v6. |
