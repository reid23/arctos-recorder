set dotenv-load

pi := "ar-sn1"

# Runs a packaging script in its image, as you, with the repository at /src.
in-packaging-image := 'docker run --rm --user "$(id -u):$(id -g)" -v "' + justfile_directory() + ':/src" -e HOME=/tmp -e CARGO_HOME=/src/target/package/cargo'

# Run the base station app.
bsw:
    cargo run --release -p bsw

# Package the base station for Ubuntu 22.04 and later, with its client id on Arctos.
package-linux client_id:
    docker build -f deploy/linux/Dockerfile -t arctos-recorder-linux .
    {{in-packaging-image}} -e ARCTOS_CLIENT_ID={{quote(client_id)}} arctos-recorder-linux sh /src/deploy/linux/package.sh

# Build the base station's Windows installer, with its client id on Arctos.
package-windows client_id:
    docker build -f deploy/windows/Dockerfile -t arctos-recorder-windows .
    {{in-packaging-image}} -e ARCTOS_CLIENT_ID={{quote(client_id)}} arctos-recorder-windows sh /src/deploy/windows/package.sh

# Build the base station's Mac app (for Intel and Apple Silicon) on a Mac, with its client id on Arctos.
package-macos client_id:
    ARCTOS_CLIENT_ID={{quote(client_id)}} deploy/macos/bundle.sh

# Package the camera software for Raspberry Pi OS.
package-csw:
    docker build -f deploy/pi/Dockerfile -t arctos-recorder-pi .
    {{in-packaging-image}} arctos-recorder-pi sh /src/deploy/pi/package.sh

# Write the cameras' Raspberry Pi OS image (CAMERA_PASSWORD sets ar's password, "ar" if unset).
pi-image: package-csw
    {{in-packaging-image}} -e CAMERA_PASSWORD arctos-recorder-pi sh /src/deploy/pi/image.sh

# Install the camera software's package on a camera (keeps its calibration).
deploy-csw host=pi: package-csw
    scp target/package/arctos-recorder-camera_$(cargo pkgid -p csw | sed 's/.*#//')_arm64.deb {{host}}:/tmp/arctos-recorder-camera.deb
    ssh -t {{host}} 'sudo apt-get install -y --reinstall /tmp/arctos-recorder-camera.deb && rm /tmp/arctos-recorder-camera.deb'

# Follow a camera's logs.
csw-logs host=pi:
    ssh {{host}} journalctl -fu csw

# Build the cloud stitching server's Docker image.
ssw-image:
    docker build -f ssw/Dockerfile -t arctos-recorder-ssw .
