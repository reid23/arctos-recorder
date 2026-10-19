# Arctos Recorder

This repository will contain the system for automatically recording
and editing Jugger games and uploading them to Arctos.

## Usage

The workspace has these crates:

- `protocol`: the types and HTTP API shared by the cameras and the base
  station.
- `arctos`: the Arctos API client.
- `stitching`: rendering recordings and stitching live previews with
  [reco](https://github.com/reco-project/video-stitcher), shared by `bsw`
  and `ssw`.
- `csw`: the camera software. Linux/libcamera only, so it is built on the
  Pi and is not a default workspace member.
- `bsw`: the base station desktop app (Dioxus). Needs `ffmpeg` on the
  `PATH`, a GPU when stitching locally, and Chromium for the live
  scoreboard overlay.
- `ssw`: the cloud stitching server, run as a Docker container on a GPU
  server (see `ssw/Dockerfile`).

Cameras find the base station by broadcasting on UDP port 7701 and then
talk to it on TCP port 7700, so both must be open on the camera network
interface. On Ubuntu:

```sh
sudo ufw allow in on <camera interface> to any port 7700 proto tcp
sudo ufw allow in on <camera interface> to any port 7701 proto udp
```

```sh
just bsw          # run the base station
just deploy-csw   # build csw on the Pi (ssh host `ar-sn1`) and run it as a service
just csw-logs     # follow its logs
just ssw-image    # build the cloud stitching server's Docker image
```

In the app's Settings, sign in with Arctos (as an organizer of the
tournament) and enter the tournament slug. Cameras show up under Cameras
while they are on the network; assign each a field and it records that
field's matches. When a match ends the camera uploads its recording, and
the base station stitches it (or has the cloud server stitch it) and
uploads it to the match on Arctos (see Jobs). The recording is deleted
from the base station only once the video is on Arctos. Each camera's
stitched live preview can be restreamed to an RTMP URL such as YouTube
Live's.

Under Workflow in the Settings:

- **Live stream** off stops all restreaming.
- **Stitch later** keeps recordings on the base station until you stitch
  them (or send them to the cloud server) from the Jobs pane.
- **Upload later** keeps stitched videos until you upload them from the
  Jobs pane.

The cloud server uploads what it stitches, so when stitching in the cloud
the two are one option, **Stitch and upload later**.

The scoreboard overlay is drawn onto the stitched videos and live
previews wherever they are stitched. Recordings use Arctos's renderings
of each state of the match's scoreboard; live previews use screenshots
of the live scoreboard page taken by headless Chromium (`$CHROMIUM`, or
`chromium`/`google-chrome` on the `PATH`; extra flags in
`$CHROMIUM_FLAGS`).

bsw and ssw talk to <https://events.californiajugger.org> unless these
are set:

| Variable | Meaning |
|---|---|
| `ARCTOS_URL` | Another Arctos server, e.g. `http://127.0.0.1:5000` |
| `ARCTOS_CF_ACCESS_CLIENT_ID` | Cloudflare Access service token id, for servers behind Cloudflare One |
| `ARCTOS_CF_ACCESS_CLIENT_SECRET` | ... and its secret |
| `ARCTOS_CLIENT_ID` | bsw's OAuth client id on that server (defaults to the one it was built with) |

To sign in, bsw must be registered on the Arctos server (Developers,
then "Your apps") as a public app with the redirect URI
`http://127.0.0.1/callback`. Build it with `ARCTOS_CLIENT_ID` set to the
app's client id to make that the default.

### Cloud stitching

Run ssw on a host with an NVIDIA GPU and the NVIDIA Container Toolkit:

```sh
SSW_TOKEN=<secret> docker compose -f ssw/compose.yaml up -d --build
```

Then, in bsw's Settings, choose "In the cloud" and enter the server URL
(`http://<host>:7800`) and the token. Recordings are then sent to the
server, which stitches them, uploads them to Arctos, and deletes them.
The server never sees your Arctos sign-in: bsw hands it short-lived
access tokens, which it only keeps in memory. Each camera's previews are
forwarded to the server too, which stitches them and restreams them to
the camera's RTMP URL. Nothing on the server needs to persist; bsw sends
again any job the server has lost.

### Calibration and cooling

A camera's stitching calibration lives at `/etc/csw/calibration.json`
on the Pi (a reco `MatchCalibration` at the recording resolution);
`deploy/calibration.json` is an uncalibrated placeholder.

The Pi needs active cooling: once it throttles, the ISP can't keep up
with two cameras at 56 fps and frames are dropped. csw logs each
camera's frame rate and dropped frames every 30 s.

## Objectives

This system aims to:
- remove the requirement for a camera operator
- eliminate tedious manual editing, compiling, and uploading of match
  footage
- make match footage available closer to real time!
  - let friends follow along and watch as it happens
  - let teams watch their games earlier (don't force them to wait
    until someone gets home and uploads)
- increase the odds that games are actually recorded!
  - reduce reliance on player-supplied phones, with their variable
    storage capacity and battery life
	
## Assumptions

ngl really unsure about which of these are reasonable

- there is a power outlet available.
- ~~sufficient phones can be sourced.~~
- The cell network has 50Mb/s of upload speed.

## Requirements Tree

todo: finish this tree. currently only have L1 requirements.

The system shall:
1. get match footage on Arctos within 30 minutes of the end of the
   match (ie, live is not necessary. live would be nice but it doesn't
   really affect our actual goals)
2. capture all play in the footage.
3. upload footage that is edited to exclude non-game time.
4. be capable of capturing and uploading five fields worth of video at
   once on a single Starlink connection
5. require less than 60 minutes of day-of setup for one person to
   record five fields.
6. cost less than $ 1000 to initially purchase for five fields (can be
   reasonably paid off by a tournament over 3-ish years)
7. cost less than $ 25 per tournament day to operate.


## Implementation Plan

There will be one central computer running a desktop app `bsw` (base
software) and acting as a router. It will connect to a PoE switch,
which connects to a recording device on each field. Each recording
device will run the `csw` (camera software) app.

The recording devices will comprise of:
- a 2.7m tripod
- a Raspberry Pi 5 (2gb) with a PoE hat
- two wide angle cameras (Camera Module 3 Wide, or equivalent)
- a usb microphone

### `csw` logic

1. wait for base station to connect. it will be available on a known
   domain.
2. make a request to the base station to register this camera.
   - send ID info, camera calibration, sync time with base station.
3. start the [low-res] stream. 1536×864 @ 30fps probably. mjpg, low
   jpg quality. integrate audio somehow.
4. poll at 1hz for match/point status on this field.

When a match is going, start recording timestamped frames to an
$n$-second ring buffer. Once a frame gets to the end of the
buffer, if it was not within $\Delta t$ of a point, discard it.

If it was a part of a match, write (append) it to a file. Importantly,
every time a frame is dropped, a new file is made. That way, every
file is contiguous video/audio. For each file, the start and end
timestamps will be saved.

Each camera will run at a resolution of 2304x1296 at 56 frames per
second and ten bit color depth. This gives a net data rate of

$$
(2304 \times 1296 \,\mathrm{pixels})\left(\frac{56\, \mathrm{frames}}{\mathrm{seconds}}\right)\left(\frac{10 \,\mathrm{bits}}{\mathrm{pixel}}\right)=1.67\mathrm{Gbps}
$$

this is a total of 3.25 Gbps for two cameras. Assuming we get at least
5x compression from jpeg (which should be easily achievable at a
reasonable quality level), this is 0.67Gbps of data to store. If we
take $\Delta t = 10\mathrm{s}$, we get that our buffer will be
836MB. We have 2gb of ram, so this should be feasible.

Once the match ends, the video (with all the points concatenated) will
be uploaded to the base station. Once the base station confirms
receipt, the files should be deleted from the camera.

### `bsw` logic

`bsw` will be either a complete dioxus app or a rust backend with a
typescript frontend connected with `ts-rs`. not sure which. Maybe the
former, since it'll be cleaner than opening a browser and running a
command.

Anyway. `bsw` will:
- poll Arctos for match data and relay it to the cameras 
- present a UI for managing the cameras, viewing the streams, and
  mapping them to fields on Arctos.
- optionally stream the low-quality live stream over rtmp
- receive match video clips and upload them to a server. This will
  either happen as soon as they are received, or at a later time (ie,
  just leave the videos on disk and then upload them at the end of the
  day).

The other key feature is panorama stitching. There will be two
operating modes: local and cloud stitching. For local stitching, `bsw`
will stitch together the low-quality streams live before streaming
them, and stitch the video clips (and concatenate them) before
uploading directly to Arctos. For cloud stitching mode, a separate
server will handle the heavy compute task of stitching and
concatenating all the videos.

In order to account for latency, we'll have to update the Arctos
scoreboard overlay (which should be optionally included in the feeds)
to have a configurable latency.

Stitching will be done with [meow](https://github.com/jarsba/meow),
[reco](https://github.com/reco-project/video-stitcher) or similar.

