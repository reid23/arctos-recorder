#!/bin/sh
# Writes the cameras' Raspberry Pi OS image, in the image built from
# deploy/pi/Dockerfile with the repository mounted at /src (see
# `just pi-image`), from the package deploy/pi/package.sh leaves. On its
# first boot a camera names itself ar-<the end of its serial number>, takes
# a self-assigned address on its Ethernet port, turns on SSH for the user ar
# and installs the camera software (see deploy/pi/user-data).
#
# CAMERA_PASSWORD sets ar's password ("ar" if unset).
set -eu

cd /src
work=target/package/pi
release=2026-10-06
os=$release-raspios-trixie-arm64-lite.img
os_sha256=483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843
version=$(cargo pkgid -p csw | sed 's/.*#//')
package=target/package/arctos-recorder-camera_${version}_arm64.deb

if [ ! -f "$work/$os.xz" ]; then
    curl -sSfL -o "$work/$os.xz.part" \
        "https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-$release/$os.xz"
    mv "$work/$os.xz.part" "$work/$os.xz"
fi
echo "$os_sha256  $work/$os.xz" | sha256sum -c --quiet
image=$work/arctos-recorder-camera.img
xz -dc "$work/$os.xz" > "$image"

partition_offset() {
    sector=$(sfdisk -d "$image" | sed -n "s|^$image$1 : start= *\([0-9]*\),.*|\1|p")
    echo $((sector * 512))
}
boot="$image@@$(partition_offset 1)"
root="$image?offset=$(partition_offset 2)"

image_libcamera=$(debugfs -R 'cat /var/lib/dpkg/status' "$root" 2>/dev/null \
    | awk '/^Package: /{package=$2} /^Version: / && package == "libcamera0.7" {print $2}')
package_libcamera=$(dpkg-deb -f "$package" Depends | sed -n 's/.*libcamera0\.7 (= \([^)]*\)).*/\1/p')
if [ "$image_libcamera" != "$package_libcamera" ]; then
    echo "Raspberry Pi OS $release has libcamera $image_libcamera, but the camera software was built with $package_libcamera (see deploy/pi/Dockerfile)." >&2
    exit 1
fi

password_hash=$(printf '%s\n' "${CAMERA_PASSWORD:-ar}" | openssl passwd -6 -stdin)
sed "s|@PASSWORD_HASH@|$password_hash|" deploy/pi/user-data > "$work/user-data"
printf 'network:\n  config: disabled\n' > "$work/network-config"
mtype -i "$boot" ::config.txt > "$work/config.txt"
printf '\n# Arctos Recorder cameras use neither Wi-Fi nor Bluetooth.\ndtoverlay=disable-wifi\ndtoverlay=disable-bt\n' >> "$work/config.txt"
printf '\n# Not every imx708 module is auto-detected.\ncamera_auto_detect=0\ndtoverlay=imx708,cam0\ndtoverlay=imx708,cam1\n' >> "$work/config.txt"
mcopy -o -i "$boot" "$work/user-data" "$work/network-config" "$work/config.txt" ::
mcopy -o -i "$boot" "$package" ::arctos-recorder-camera.deb
rm "$work/user-data" "$work/network-config" "$work/config.txt"

xz -T0 -c "$image" > "target/package/arctos-recorder-camera-$version.img.xz"
rm "$image"
