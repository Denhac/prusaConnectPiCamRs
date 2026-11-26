# The Pi's camera API

A read-only HTTP API on each Pi, for programs that fetch its cameras' pictures, such as a
prusa-connect module that posts them to Slack. Turn it on with `deploy/deploy.sh api HOST`. It
listens on port 8555 and answers at `http://<Pi>:8555/v1`.

- **Access:** there is no password. Clients on a private network may connect: the LAN, the Pi
  itself, and Tailscale. Clients from public internet addresses are refused.
- **Requests:** only GET and HEAD. Each response closes its connection.
- **Errors:** RFC 9457 problem documents (`application/problem+json`) with a `code` to branch
  on, `detail` in prose for people, and `retryable`.
- **Versioning:** this is v1. New fields may appear in JSON answers and new values in `state`, so
  a client ignores fields it doesn't know. Anything else that changes gets a new version.

## `GET /v1/cameras`

```json
{
  "cameras": [
    {
      "id": "cam0",
      "name": "camera-pi-5",
      "state": "stills",
      "still": {
        "available": true,
        "seq": 812,
        "captured_at": "2026-11-22T19:10:01Z",
        "age_ms": 640
      },
      "video": null,
      "rtsp": "rtsp://10.0.0.36:8554/cam0"
    }
  ]
}
```

| field | meaning |
|---|---|
| `state` | `stills`: the camera is taking stills. `video`: someone is watching live, and stills pause. Treat a value you don't know as `stills`. |
| `still.available` | Whether `/still` with no parameters would answer 200 right now. |
| `still.seq` | The newest still's number. It increases with each still; it is also the still's ETag. `null` before the first still. |
| `still.captured_at`, `still.age_ms` | When the newest still was taken, by the Pi's clock, and how long ago. |
| `video` | While `state` is `video`: `width`, `height`, `fps`, and `viewers`, the number of viewers sharing the stream. |
| `rtsp` | The live stream's address, when the Pi serves one: the address the RTSP server is bound to, or else the one your request reached the Pi on. It is never a Tailscale address: a request over Tailscale gets the Pi's LAN address. |

## `GET /v1/cameras/{id}`

One camera, as above. An unknown id is 404 `camera_not_found`.

## `GET /v1/cameras/{id}/still`

The newest still as `image/jpeg`.

| parameter | default | meaning |
|---|---|---|
| `max_age` | 30 | The oldest still to accept, in seconds (at most 86400). |
| `wait` | 0 | Wait up to this many seconds (at most 30) for a still taken after the request, instead of answering with the newest. |

**Answers:**
- **200:** the JPEG, with headers:
  - `ETag: "<seq>"`
  - `Last-Modified`
  - `Cache-Control: no-cache`

  Send `If-None-Match` with the ETag to get **304** instead when nothing newer has been taken.
- **503** `still_unavailable`, with `Retry-After` in seconds and `retryable: true`. No still fits:
  someone is watching live and stills are paused, the camera hasn't taken one yet, or the newest
  is older than `max_age`.
- **404** `camera_not_found`.
- **400** `bad_request`: a parameter isn't a whole number.

Stills are as large as the camera's still settings make them (for example 2304x1296). Their size
may change: stills taken from live video, when that comes, will have the live stream's size. A
client must not assume one size, and should treat a 503 as "try again later", since a later
version may answer the same request with 200.

## `GET /v1/status`

What each of the Pi's outputs is doing, the same report `prusa-connect-camera status` prints:

```json
{
  "pid": 1234,
  "outputs": {
    "cam0/cloud": { "state": "ok", "message": "uploading every 15s", "since": 1795374601 },
    "rtsp": { "state": "ok", "message": "listening on 0.0.0.0:8554, 0 watching", "since": 1795374590 }
  }
}
```

`state` is one of `starting`, `ok`, `paused`, `auth` (a server refused the credentials) and
`retrying`. `since` is in seconds since 1970.

## `GET /v1`

The service's name and version, and the list of endpoints.

## Examples

```shell
curl -s http://10.0.0.36:8555/v1/cameras
curl -s -o xl.jpg http://10.0.0.36:8555/v1/cameras/cam0/still
curl -s -o xl.jpg 'http://10.0.0.36:8555/v1/cameras/cam0/still?wait=10'
```
