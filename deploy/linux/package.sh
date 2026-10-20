#!/bin/sh
# Builds the base station and packages it for Ubuntu, in the image built
# from deploy/linux/Dockerfile with the repository mounted at /src (see
# `just package-linux`). Leaves the package in target/package.
set -eu

cd /src
work=target/package/linux
cargo build --release --locked -p bsw --target-dir "$work/cargo"
version=$(cargo pkgid -p bsw | sed 's/.*#//')

root=$work/root
rm -rf "$root"
install -Dm755 "$work/cargo/release/bsw" "$root/usr/lib/arctos-recorder/bsw"
strip "$root/usr/lib/arctos-recorder/bsw"
install -Dm755 /opt/ffmpeg/ffmpeg "$root/usr/lib/arctos-recorder/ffmpeg"
mkdir -p "$root/usr/bin"
ln -s ../lib/arctos-recorder/bsw "$root/usr/bin/arctos-recorder"
install -Dm644 deploy/linux/arctos-recorder.desktop "$root/usr/share/applications/arctos-recorder.desktop"
install -Dm644 bsw/assets/icon.png "$root/usr/share/icons/hicolor/256x256/apps/arctos-recorder.png"
install -Dm644 deploy/linux/copyright "$root/usr/share/doc/arctos-recorder/copyright"

# dpkg-shlibdeps reads the package's name from a debian/control.
mkdir -p "$work/shlibs/debian"
printf 'Source: arctos-recorder\n\nPackage: arctos-recorder\nArchitecture: any\n' > "$work/shlibs/debian/control"
depends=$(cd "$work/shlibs" && dpkg-shlibdeps -O \
    "/src/$root/usr/lib/arctos-recorder/bsw" "/src/$root/usr/lib/arctos-recorder/ffmpeg")

mkdir -p "$root/DEBIAN"
install -m755 deploy/linux/postinst deploy/linux/postrm "$root/DEBIAN/"
cat > "$root/DEBIAN/control" <<EOF
Package: arctos-recorder
Version: $version
Architecture: amd64
Maintainer: reid23 <reid.dye@gmail.com>
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: ${depends#shlibs:Depends=}, libvulkan1
Recommends: network-manager, mesa-vulkan-drivers
Section: video
Priority: optional
Description: Arctos Recorder base station
 Manages the cameras around jugger fields, and stitches, streams and
 uploads their footage to Arctos.
EOF

dpkg-deb --root-owner-group --build "$root" "target/package/arctos-recorder_${version}_amd64.deb"
