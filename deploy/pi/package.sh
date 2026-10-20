#!/bin/sh
# Cross-compiles the camera software and packages it for Raspberry Pi OS,
# in the image built from deploy/pi/Dockerfile with the repository mounted
# at /src (see `just package-csw`). Leaves the package in target/package.
set -eu

cd /src
work=target/package/pi
target=aarch64-unknown-linux-gnu
cargo build --release --locked -p csw --target $target --target-dir "$work/cargo"
version=$(cargo pkgid -p csw | sed 's/.*#//')

root=$work/root
rm -rf "$root"
install -Dm755 "$work/cargo/$target/release/csw" "$root/usr/bin/csw"
aarch64-linux-gnu-strip "$root/usr/bin/csw"
install -Dm644 deploy/csw.service "$root/usr/lib/systemd/system/csw.service"
install -Dm644 deploy/calibration.json "$root/usr/share/csw/calibration.json"

# The cameras' tuning, without temporal denoise: it has the ISP, which both
# cameras share, read and write a whole extra frame for each one, so they
# drop frames whenever anything else uses much memory bandwidth. libcamera
# looks in LIBCAMERA_IPA_CONFIG_PATH (see deploy/csw.service) before its
# own tuning, which this is made from: the package depends on this very
# libcamera.
tuning=rpi/pisp/imx708_wide.json
install -d "$root/usr/share/csw/libcamera/$(dirname $tuning)"
jq '(.algorithms[] | select(has("rpi.denoise"))) |= walk(if type == "object" then del(.tdn) else . end)' \
    "/usr/share/libcamera/ipa/$tuning" > "$root/usr/share/csw/libcamera/$tuning"

# dpkg-shlibdeps reads the package's name from a debian/control. libcamera
# has to be the very build csw was built with (see deploy/pi/Dockerfile).
mkdir -p "$work/shlibs/debian"
printf 'Source: arctos-recorder-camera\n\nPackage: arctos-recorder-camera\nArchitecture: any\n' > "$work/shlibs/debian/control"
libcamera=$(dpkg-query -W -f='${Version}' libcamera0.7:arm64)
depends=$(cd "$work/shlibs" && dpkg-architecture -a arm64 -c dpkg-shlibdeps -O "/src/$root/usr/bin/csw" \
    | sed "s/^shlibs:Depends=//; s/libcamera0\.7 ([^)]*)/libcamera0.7 (= $libcamera)/")

mkdir -p "$root/DEBIAN"
install -m755 deploy/pi/postinst deploy/pi/prerm "$root/DEBIAN/"
cat > "$root/DEBIAN/control" <<EOF
Package: arctos-recorder-camera
Version: $version
Architecture: arm64
Maintainer: reid23 <reid.dye@gmail.com>
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: $depends
Section: video
Priority: optional
Description: Arctos Recorder camera software
 Records a jugger field with a Raspberry Pi's two cameras and sends the
 footage to the base station.
EOF

dpkg-deb --root-owner-group --build "$root" "target/package/arctos-recorder-camera_${version}_arm64.deb"
