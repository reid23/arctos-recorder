pi := "ar-sn1"

# Run the base station app.
bsw:
    cargo run --release -p bsw

# Build csw on a camera and (re)start it as a service (keeps its calibration).
deploy-csw host=pi:
    rsync -az --delete --exclude target --exclude .git ./ {{host}}:arctos-recorder/
    ssh -t {{host}} 'cd arctos-recorder \
        && ~/.cargo/bin/cargo build --release -p csw \
        && sudo install target/release/csw /usr/local/bin/csw \
        && sudo install -m644 deploy/csw.service /etc/systemd/system/csw.service \
        && (test -e /etc/csw/calibration.json || sudo install -Dm644 deploy/calibration.json /etc/csw/calibration.json) \
        && sudo systemctl daemon-reload \
        && sudo systemctl enable csw \
        && sudo systemctl restart csw'

# Follow a camera's logs.
csw-logs host=pi:
    ssh {{host}} journalctl -fu csw

# Build the cloud stitching server's Docker image.
ssw-image:
    docker build -f ssw/Dockerfile -t arctos-recorder-ssw .
