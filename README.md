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
- The cell network is sufficiently strong to upload this data.

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
