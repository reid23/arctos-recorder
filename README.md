# Arctos Recorder

This repository will contain the system for automatically recording
and editing Jugger games and uploading them to Arctos.


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

- there is a power outlet available.
- The cell network has 50Mb/s of upload speed.

## Requirements

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
6. have a BOM cost less than $ 1250 to initially purchase for five
   fields (can be reasonably paid off by a tournament over 3-ish years)
8. cost less than $ 25 per tournament day to operate.
9. not require any existing field infrastructure besides the two bullets
   listed in [[#Assumptions]]

### Initial Decisions/Conclusions

A couple of things were immediately ruled out by these requirements:
- using player-provided phones. This is a lot of risk since we won't
  have hardware access for testing.
- single cameras: to get the whole field in view, the mount must be
  incredibly tall, making it very expensive and difficult to
  construct.
- separate cell uplinks at every field: this is a lot of hardware to
  purchase, plus a lot of recurring cost to pay for cell plans.
- non-centralized processing: generally, the more compute we can
  combine into the base station, the the system will be, since we
  won't have to buy a copy of the hardware for each field.

Given these, I decided on the architecture described below.

## System Overview

There is one central computer running the `bsw` (base software) app.
It is connected to a PoE (Power over Ethernet) switch, which has
ethernet cables to a camera rig on each field. The camera rigs run
the `csw` (camera software) app.

The camera rigs will be mounted on 2.7m tripods and spaced at least 1m
away from the edge of the field. They will have:
- a Raspberry Pi 5 with 2GB+ RAM
- a PoE HAT
- two wide angle CSI cameras
- a USB microphone
- a cooling system (copecopecope)

The mounting height and distance were chosen based on the availability of cost-effective tripods; a different height can work but may require a different minimum distance from the field and different camera positions. 

The system looks like this:
<img width="1440" height="1920" alt="image" src="https://github.com/user-attachments/assets/18a3816f-19eb-4d7d-87cd-c3da7d7b8fed" />

### BOM

The full hardware BOM is as follows:

| part | qty | cost (ea) | source |
|------|-----|-----------|--------|
|Pi 5 2GB|1/field|77.50|[PiShop](https://www.pishop.us/product/raspberry-pi-5-2gb/)|
|Pi 5 PoE HAT|1/field|20.95|[PiShop](https://www.pishop.us/product/poe-hat-g-compatible-with-raspberry-pi-5-cm5-5v-5a-output-802-3af-at/)|
|wide angle CSI camera|2/field|30.00|[Amazon](https://www.amazon.com/dp/B0C5D97DRJ)|
|RPi active cooler|1/field|5.00|[DigiKey](https://www.digikey.com/en/products/detail/raspberry-pi/SC1148/21658255)|
|USB microphone|1/field|8.99|[Amazon](https://a.co/d/05wimx7k)|
|100ft ethernet cable|1/field|21.99|[Amazon](https://a.co/d/095YrWB3)|
|2.7m tripod|1/field|59.99|[Amazon](https://a.co/d/047WDrRC)|
|M3x12mm screw|5/field|0.0587|[McMaster](https://www.mcmaster.com/91290A500/)|
|M2x12mm screw|8/field|0.0587|[McMaster](https://www.mcmaster.com/91292A834/)|
|M2 hex nut|8/field|0.0306|[McMaster](https://www.mcmaster.com/90591A265/)|
|M2 washer|8/field|0.0272|[McMaster](https://www.mcmaster.com/93475A195/)|
|1/4"-#20 heat-set threaded insert|1/field|0.25|[Amazon](https://a.co/d/0e3IGgWV)|
|8-port 120W PoE Switch|1|39.99|[Amazon](https://a.co/d/02OrRrvX)|

plus 3D printed components (CAD coming soon).

This gives a theoretical BOM cost of $255.89 per field, plus the 
39.99 PoE switch. This *is* over the target budget, but not by that 
much, and I am currently investigating the feasibility of using a 1GB 
Pi 5, which would bring us *just* under the target cost.

### High-Level `csw` logic

1. wait for base station to connect. it will be available on a known
   domain.
2. make a request to the base station to register this camera.
   - send ID info, camera calibration, sync time with base station.
3. start the [low-res] stream. 1536×864 @ 30fps is typical. mjpg, low
   jpg quality. 
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

Once the match ends, the video (with all the points concatenated) gets
uploaded to the base station. Once the base station confirms receipt, 
the files are deleted from the camera.

### `bsw` logic

`bsw` will:
- poll Arctos for match data and relay it to the cameras 
- present a UI for:
    - calibrating the cameras
    - viewing the streams
    - mapping cameras to fields on Arctos
- optionally stream the low-quality live stream over rtmp
- receive match video clips and upload them to a server. This will
  either happen as soon as they are received, or at a later time (ie,
  just leave the videos on disk and then upload them at the end of the
  day).

The other key feature is panorama stitching. There are two
operating modes: local and cloud stitching. For local stitching, `bsw`
stitches the low-quality streams live before streaming them, and stitches 
the video clips before uploading directly to Arctos. For cloud stitching 
mode, a separate server handles the heavy compute task of stitching and
concatenating all the videos. This helps when the base station has no GPU,
but also when the base station must not consume a ton of power (for 
example, if the system is powered by a portable battery)

### Algorithms

Okay so there's all the plumbing but there are also some nontrivial
algorithms to get this stitching to work. I'm totally stealing all
this from the 106a labs that Kush developed this semester plus cs180
lab2. i've been relearning all the basic CV stuff so i dont look
clueless when students ask for help...

Anyway. here's what happens live as we stitch:

1. Undistort. Just the OpenCV fisheye model. (fit with the normal
   checkerboard calibration. this calibration also gives the intrinsic
   camera parameters.)
   - implemented a custom optimizer for this (just L-M, damped
     newton's method) bc opencv was so ass??
2. Reproject onto a single cylindrical image "plane"
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


## Usage & Details

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
uploads it to the match on Arctos (see Jobs). One recording can be
stitched while another uploads, and stitching gives way to the live
previews, so it slows down while they are being stitched. The recording
is deleted from the base station only once the video is on Arctos. Each camera's
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
stitched videos are compressed, wherever they are stitched. **Before
points** and **After points** are how much of what happens around each
point the videos include. The cameras hold every frame for **Camera
buffer** before keeping or discarding it, so it has to be longer than
Before points by however long Arctos takes to tell them a point has
started; a longer one takes more of their memory.

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
(`http://<host>:7800`, or `https://` behind a TLS proxy) and the token.
Recordings go up in chunks of at most 64 MiB, so the server can sit
behind Cloudflare's proxy or a Cloudflare Tunnel, which reject request
bodies over 100 MB. Recordings are then sent to the
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
calibration at `/etc/csw/calibration.json`, and is sent to the cloud
server along with live previews and recordings.

The camera keeps this setup, along with its swapped sides, microphone,
lens positions and output shape, in `/var/lib/csw/setup.json`, so any
base station it connects to stitches it the same way. When a camera
connects, its setup replaces the base station's copy in `settings.json`;
a camera that keeps none is given the base station's copy. Each base
station keeps its own field, stream server and stream key for a camera,
and the chessboard pictures and alignment its Calibration window starts
from.

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

Wi-Fi and Bluetooth are turned off, and the two cameras are set up as
imx708 sensors on the Pi's two camera ports rather than auto-detected. To
update a camera's software, `just deploy-csw ar@ar-<serial>.local`.

Raspberry Pi changes libcamera's ABI without changing its soname, so the
camera software is built against the image's very libcamera, and its
package won't install alongside another. Moving to a newer Raspberry Pi
OS means changing the release in `deploy/pi/image.sh` and the libcamera
version and checksums in `deploy/pi/Dockerfile` together. The package
carries that libcamera's tuning for the cameras with temporal denoise
taken out, as it costs the ISP more memory bandwidth than two cameras
leave (see `deploy/pi/package.sh`).
