# Arctos Recorder

This repository will contain the system for automatically recording
and editing Jugger games and uploading them to Arctos.

## Usage

The workspace has these crates:

- `protocol`: the types and HTTP API shared by the cameras and the base
  station.
- `arctos`: the Arctos API client.
- `stitching`: rendering recordings and stitching live previews on the
  GPU, shared by `bsw` and `ssw`.
- `calibration`: fitting lenses to pictures of a chessboard and aligning
  a rig's cameras from what they both see (OpenCV), for `bsw`.
- `csw`: the camera software. Linux/libcamera only, so it is
  cross-compiled for the Pi in Docker (see Packaging) and is not a
  default workspace member.
- `bsw`: the base station desktop app (Dioxus). Building it needs the
  OpenCV development files and libclang (`libopencv-dev libclang-dev` on
  Ubuntu). Running it from source needs `ffmpeg` on the `PATH` (the
  packages bring their own, next to the executable) and a GPU when
  stitching locally.
- `ssw`: the cloud stitching server, run as a Docker container on a GPU
  server (see `ssw/Dockerfile`).

```sh
just bsw          # run the base station
just deploy-csw   # install the camera software's package on the Pi (ssh host `ar-sn1`)
just csw-logs     # follow its logs
just ssw-image    # build the cloud stitching server's Docker image
```

### The camera network

The cameras and the base station share a wired network with no router:
every device gives itself a self-assigned (link-local, 169.254.x.x)
address. Cameras find the base station by broadcasting on UDP port 7701
and then talk to it on TCP port 7700.

Windows and macOS give a port a self-assigned address by themselves. On
Linux, NetworkManager instead keeps waiting for a DHCP server, so choose
the port the cameras are plugged into under Settings > Camera network;
bsw gives that port a profile of its own ("Arctos Recorder cameras").

The packages open the two ports to self-assigned addresses only: the
Ubuntu package adds ufw rules if ufw is installed, and the Windows
installer adds a firewall rule for bsw. macOS asks to allow local
network access the first time bsw runs.

In the app's Settings, sign in with Arctos (as an organizer of the
tournament) and enter the tournament slug. Cameras show up under Cameras
while they are on the network; assign each a field and it records that
field's matches. When a match ends the camera uploads its recording, and
the base station stitches it (or has the cloud server stitch it) and
uploads it to the match on Arctos (see Jobs). The recording is deleted
from the base station only once the video is on Arctos. Each camera's
stitched live preview can be restreamed to an RTMP server such as YouTube
Live's: enter the server's URL and the stream key it gives you in the
camera's box.

Under Resource Usage in the Settings:

- **Live stream** off stops all restreaming. While it is on, Live Stream
  sets the width of the stitched live previews and the streams' bitrate,
  and the size and JPEG quality of the previews the cameras send, which
  are stitched into them. Bigger, finer camera frames make sharper streams
  but take more of the cameras' CPU and of the network. A camera recording
  a match changes size once the match ends, and its previews pause for a
  moment while it does.
- **Stitch later** keeps recordings on the base station until you stitch
  them (or send them to the cloud server) from the Jobs pane.
- **Upload later** keeps stitched videos until you upload them from the
  Jobs pane.

The cloud server uploads what it stitches, so when stitching in the cloud
the two are one option, **Stitch and upload later**.

Under Recordings, **JPEG quality** is how finely the cameras compress the
frames they record (higher takes more of their CPU, so they may drop
frames, and makes bigger uploads), and **Video quality** is how finely
stitched videos are compressed, wherever they are stitched.

Window > Performance opens a pane on the left showing this computer's
CPU, memory and NVIDIA GPU use, and each online camera's CPU, memory,
temperature and the share of frames it lost over the last 10 seconds.

The scoreboard overlay is drawn onto the stitched videos and live
previews wherever they are stitched. Recordings use Arctos's renderings
of each state of the match's scoreboard; live previews use screenshots
of the live scoreboard page taken by a headless browser: `$CHROMIUM` if
set, otherwise an installed Chrome, Chromium or Edge, otherwise Chrome's
headless shell, which bsw downloads the first time it needs it. Snap
installs (Ubuntu's Chromium) are passed over for the download. Extra
browser flags go in `$CHROMIUM_FLAGS`.

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

A camera sends the base station the calibration at
`/etc/csw/calibration.json` on the Pi (a `stitching::geometry::Calibration`
at the recording resolution); `deploy/calibration.json` holds the lenses'
nominal values and the cameras' nominal mount.

To calibrate a camera from the base station, press Calibration… on the
camera. Its window walks through
focusing the lenses, fitting each lens to pictures of a chessboard,
entering how the cameras are mounted, aligning them from what they both
see, and choosing the view and blend. What it saves overrides the
camera's calibration on this base station, and is sent to the cloud
server along with live previews and recordings.

A camera autofocuses the first time it connects and whenever Focus is
pressed (once it isn't recording); its lenses are then held there. A
lens magnifies the image by about 0.18% per unit of libcamera's lens
position as it focuses closer, so each lens's calibration records the
position it was measured at and is rescaled to where the lens is held.
Calibrations from before focus was recorded count as measured at 1.0,
where libcamera held the lenses until then.

The Pi needs active cooling: once it throttles, the ISP can't keep up
with two cameras at 56 fps and frames are dropped. csw logs each
camera's frame rate and dropped frames every 30 s.

### Packaging

Everything is built on Linux in Docker except the Mac app: Apple's SDK
may only be used on Apple hardware, so `deploy/macos/bundle.sh` runs on
a Mac, Intel or Apple Silicon, and builds one app for both (it needs
Xcode's command line tools, Rust, CMake and NASM). The base station
builds take the client id of bsw's app on
Arctos (see above), which the app signs in with unless `ARCTOS_CLIENT_ID`
is set when it runs.

```sh
just package-linux <client id>    # target/package/arctos-recorder_<version>_amd64.deb
just package-windows <client id>  # target/package/arctos-recorder-<version>-setup.exe
just package-macos <client id>    # target/package/ArctosRecorder-<version>.dmg (on the Mac)
just package-csw                  # target/package/arctos-recorder-camera_<version>_arm64.deb
just pi-image                     # target/package/arctos-recorder-camera-<version>.img.xz
```

- The Ubuntu package is built on Ubuntu 22.04 and installs on 22.04 and
  later: `sudo apt install ./arctos-recorder_<version>_amd64.deb`.
- The Windows installer uses the WebView2 runtime that comes with
  Windows 10 and 11.
- The Mac app is signed ad hoc rather than with a developer
  certificate, so macOS asks to approve it (System Settings > Privacy &
  Security > Open Anyway) and to allow local network access, again
  after each update.

The base station packages link OpenCV statically (`deploy/opencv.sh`)
and bring their own ffmpeg: 8.1 on Linux and Windows, since ffmpeg 9's
NVENC needs NVIDIA's 610 driver or later.

### Camera image

`just pi-image` writes Raspberry Pi OS Lite (trixie) for a Pi 5 with the
camera software. Write it to the SD card with `xzcat <image> | sudo dd
of=/dev/<card> bs=4M`, or with Raspberry Pi Imager's "Use custom"
without OS customisation, which would replace the image's first-boot
setup. On its first boot a camera:

- names itself `ar-` and the last six hex digits of its serial number;
  the base station knows the camera by this name;
- gives its Ethernet port a self-assigned address;
- turns on SSH for the user `ar`, with the password `ar` (or
  `CAMERA_PASSWORD` when the image was written): `ssh
  ar@ar-<serial>.local`;
- installs the camera software with the nominal calibration.

Wi-Fi and Bluetooth are turned off. To update a camera's software, `just
deploy-csw ar@ar-<serial>.local`.

Raspberry Pi changes libcamera's ABI without changing its soname, so the
camera software is built against the image's very libcamera, and its
package won't install alongside another. Moving to a newer Raspberry Pi
OS means changing the release in `deploy/pi/image.sh` and the libcamera
version and checksums in `deploy/pi/Dockerfile` together.

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
6. cost less than $ 1250 to initially purchase for five fields (can be
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

## Algorithms

Okay so there's all the plumbing but there are also some nontrivial
algorithms to get this stitching to work. I'm totally stealing all
this from the 106a labs that Kush developed this semester plus cs180
lab2. i've been relearning all the basic CV stuff so i dont look
clueless when students ask for help...

Anyway. here's what happens live as we stitch:

1. Undistort. cylindrical distortion model. Just through OpenCV. this
   calibration also gives the intrinsic camera parameters.
   - implemented a custom optimizer for this (just L-M, damped
     newton's method) bc opencv was so ass??
2. Reproject onto a single image plane
3. stitch the two sides together along a seam in the middle of the
   overlap. Use multiresolution blending as described by [Burt and Adelson, 1983](https://www.cs.princeton.edu/courses/archive/fall05/cos429/papers/burt_adelson.pdf).

In order to figure out the cameras' orientations relative to each
other, there's also a whole cailbration procedure:

1. undistort
2. use SIFT to find keypoints
3. run RANSAC to align the image planes in roll, pitch, yaw, and
   relative scale. (this does make it deviate from the fitted focal
   length from the checkerboard calibration, but that's okay because
   this lets us ensure that they actually match correctly. if the
   scale was off by even just a little it looked really bad)
   
Yay!

And through the magic of `claude-opus-5-5-xhigh`, I can convert my
horribly non-performant python code from lab into beautiful
GPU-optimized Rust! How wonderful!
