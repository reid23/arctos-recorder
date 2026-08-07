# Arctos Recorder

This repository will contain the system for automatically recording
and editing Jugger games and uploading them to Arctos.

Currently this project is in the design phase, so this space will just
be used for documentation.

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
- sufficient phones can be sourced.
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
6. cost less than $1000 to initially purchase for five fields (can be
   reasonably paid off by a tournament over 3-ish years)
7. cost less than $25 per tournament day to operate.


## Implementation Plan

For each field:
- two phones at the top of a pole
- power & internet over PoE: one PoE cable into the top of the
  pole. split into two usb-c cables to get power and internet to two
  phones.
- long poe cables from side of field to data tent
- computer, poe injector router, and cell uplink in data tent

Each phone will run the Arctos Recorder mobile app. The base station will
run the Arctos Recorder Base Station desktop app.

### Mobile App Logic

```
IF base station THEN
	config comes from base station.
	record matches locally. record the whole thing. upload to base station when they're done. 
	when not recording a match, poll somewhere for a frame request. base station can ask for preview and get sent images at up to like 1 fps, or ask for a video of a specified length.
ELSE
	config comes from Arctos (require login)
	while not recording, poll somewhere for a frame request. (same thing where arctos can also ask for a video of a specified length) the user can go to their profile and see all their active cameras and request slow previews. same place that they can manage their cameras and set them up.
	record matches locally.
	clip to points.
	IF upload_automatically THEN
		upload points to Arctos
	ELSE
		save points locally. allow user to upload when they want to. 
	END
END
```

when a base station is connected (poll a heartbeat fairly regularly), talk to the base station for:
- config
- video upload
- frame requests
- video sample requests

talk to arctos for:
- match/point start/end times
- if logged in, saying "im here but am connected to the base station, not arctos"

if the base station disconnects, keep going as normal, but switch to
arctos mode. (if logged in. if not logged in, just store the data
locally until the base station reconnects and you can upload).

config is:
- camera ID of some sort (intrinsic to device? we should have a human name and a UUID. the human name should be based on the device name or something, and the uuid is the actual key. uuid should never change, even when restarting the app.)
- event
- field
- upload automatically? (if there's no base station, should i auto-upload to arctos?)
- a/v settings: resolution, framerate, bitrate, encoding, which camera, etc

video metadata is:
- camera ID
- start time (system clock)
- start time (arctos clock)
- start time (base station clock) (if base station exists)
- event
- field
- match
- optional, but add as many as possible:
  - camera location (gps)
  - camera pose (imu+magnetometer)
  
  
### Base Station Functionality

The base station will have basically the same config section as would
be available on arctos if not using a base station:

- show all connected devices in a table. their rows should drop down to display a preview.
- if gps & pose info is available, plot the devices on a map.
- set each camera's field & event, and change the human name (which is what its labele as)
- set up paired cameras: where we want to fuse footage from.
- see status: recording vs idle, how much storage space there is, the status of any uploads, etc.
- [arctos only] set 'upload automatically' flag

The functionality is slightly different. On the base station, when a
phone uploads a match, the process will be:
0. save the video to disk
1. add this video and its data to some sort of "to be processed" pile

then there's a separate thing that monitors this pile, and looks for
match videos that are ready to be processed. (if a camera is paired
with another, the videos shouldn't count as ready until both are
uploaded)

1. clip to just points + buffer
2. if its a pair, use [meow](https://github.com/jarsba/meow) or
   [reco](https://github.com/reco-project/video-stitcher) or similar
   to fuse the two into one. 
3. concatenate the points together into one video, remeasuring the
   length of each clip post-fuse. compute the timestamps.
4. upload to arctos via the video api. arctos will handle uploading to
   youtube.
   
