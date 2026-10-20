#!/bin/sh
# Builds the base station as a Mac app and disk image for both Apple
# Silicon and Intel Macs, on either (Apple's tools and SDK only run on
# Macs):
#
#   ARCTOS_CLIENT_ID=<client id> deploy/macos/bundle.sh
#
# Needs Xcode's command line tools (`xcode-select --install`), Rust
# (https://rustup.rs), CMake and NASM (e.g. `brew install cmake nasm`).
# Leaves the disk image in target/package.
#
# The app is signed ad hoc rather than with a developer certificate, so a
# Mac that downloads it asks to approve it under System Settings > Privacy
# & Security (Open Anyway), and asks again for the local network after each
# update.
set -eu

cd "$(dirname "$0")/../.."
work=target/package/macos
deployment_target=12.0
ffmpeg_arm64_url=https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip
ffmpeg_arm64_sha256=c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924
ffmpeg_x86_64_url=https://ffmpeg.martin-riedl.de/download/macos/amd64/1789931006_9.0.2/ffmpeg.zip
ffmpeg_x86_64_sha256=7c6b4125b191cbf773832dc51f424cf2b6bb7da43007d1e066f95909e47cacd4

[ "$(uname -s)" = Darwin ] || { echo "Run this on a Mac." >&2; exit 1; }
xcode-select -p > /dev/null || { echo "Install Xcode's command line tools: xcode-select --install" >&2; exit 1; }
for tool in rustup cargo cmake nasm curl; do
    command -v $tool > /dev/null || { echo "Install $tool first." >&2; exit 1; }
done
[ -n "${ARCTOS_CLIENT_ID:-}" ] || { echo "Set ARCTOS_CLIENT_ID to bsw's client id on Arctos." >&2; exit 1; }
export MACOSX_DEPLOYMENT_TARGET=$deployment_target
rustup target add aarch64-apple-darwin x86_64-apple-darwin

mkdir -p "$work"
for slice in arm64:aarch64-apple-darwin x86_64:x86_64-apple-darwin; do
    architecture=${slice%%:*}
    target=${slice#*:}
    opencv="$PWD/$work/opencv-$architecture"
    # OpenCV picks its SIMD code by CMAKE_SYSTEM_PROCESSOR, which is
    # otherwise this Mac's.
    if [ ! -e "$opencv/lib/libopencv_core.a" ]; then
        deploy/opencv.sh "$opencv" -DCMAKE_SYSTEM_NAME=Darwin -DCMAKE_SYSTEM_PROCESSOR=$architecture \
            -DCMAKE_OSX_ARCHITECTURES=$architecture
    fi
    OPENCV_INCLUDE_PATHS="$opencv/include/opencv4" \
    OPENCV_LINK_PATHS="$opencv/lib,$opencv/lib/opencv4/3rdparty" \
    OPENCV_LINK_LIBS=static=opencv_calib3d,static=opencv_features2d,static=opencv_flann,static=opencv_imgproc,static=opencv_core,static=zlib \
        cargo build --release --locked -p bsw --target $target --target-dir "$work/cargo"
done
version=$(cargo pkgid -p bsw | sed 's/.*#//')

if [ ! -x "$work/ffmpeg" ]; then
    download_ffmpeg() {
        curl -sSfL -o "$work/ffmpeg-$1.zip" "$2"
        echo "$3  $work/ffmpeg-$1.zip" | shasum -a 256 -c
        unzip -o -q "$work/ffmpeg-$1.zip" ffmpeg -d "$work/ffmpeg-$1"
    }
    download_ffmpeg arm64 "$ffmpeg_arm64_url" "$ffmpeg_arm64_sha256"
    download_ffmpeg x86_64 "$ffmpeg_x86_64_url" "$ffmpeg_x86_64_sha256"
    lipo -create -output "$work/ffmpeg" "$work/ffmpeg-arm64/ffmpeg" "$work/ffmpeg-x86_64/ffmpeg"
    rm -r "$work"/ffmpeg-arm64* "$work"/ffmpeg-x86_64*
fi

app="$work/Arctos Recorder.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/bsw" \
    "$work/cargo/aarch64-apple-darwin/release/bsw" "$work/cargo/x86_64-apple-darwin/release/bsw"
cp "$work/ffmpeg" "$app/Contents/MacOS/"
sed "s/@VERSION@/$version/g; s/@DEPLOYMENT_TARGET@/$deployment_target/g" deploy/macos/Info.plist \
    > "$app/Contents/Info.plist"

iconset="$work/icon.iconset"
rm -rf "$iconset"
mkdir "$iconset"
for icon in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 256x256:256; do
    pixels=${icon#*:}
    sips -z "$pixels" "$pixels" bsw/assets/icon.png --out "$iconset/icon_${icon%:*}.png" > /dev/null
done
iconutil -c icns -o "$app/Contents/Resources/icon.icns" "$iconset"

codesign --force --sign - "$app"

image="$work/image"
rm -rf "$image"
mkdir "$image"
cp -R "$app" "$image/"
ln -s /Applications "$image/Applications"
hdiutil create -quiet -ov -volname "Arctos Recorder" -srcfolder "$image" -format UDZO \
    "target/package/ArctosRecorder-$version.dmg"
echo "target/package/ArctosRecorder-$version.dmg"
