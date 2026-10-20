#!/bin/sh
# Cross-compiles the base station for Windows and builds its installer, in
# the image built from deploy/windows/Dockerfile with the repository
# mounted at /src (see `just package-windows`). Leaves the installer in
# target/package.
set -eu

cd /src
work=target/package/windows
target=x86_64-pc-windows-gnu
cargo build --release --locked -p bsw --target $target --target-dir "$work/cargo"
version=$(cargo pkgid -p bsw | sed 's/.*#//')

app=$work/app
rm -rf "$app"
mkdir -p "$app"
cp "$work/cargo/$target/release/bsw.exe" /opt/ffmpeg/ffmpeg.exe "$app/"
x86_64-w64-mingw32-strip "$app/bsw.exe"
cp "$(ls -t "$work"/cargo/$target/release/build/webview2-com-sys-*/out/x64/WebView2Loader.dll | head -1)" "$app/"
cp /opt/ffmpeg/LICENSE.txt "$app/ffmpeg-license.txt"

# MinGW's runtime libraries, which link against each other too.
for pass in 1 2 3; do
    for dll in $(x86_64-w64-mingw32-objdump -p "$app"/*.exe "$app"/*.dll | sed -n 's/^\s*DLL Name: //p' | sort -u); do
        found=$(x86_64-w64-mingw32-g++-posix -print-file-name="$dll")
        if [ "$found" != "$dll" ] && [ ! -e "$app/$dll" ]; then
            cp "$found" "$app/"
        fi
    done
done

icotool -c -o "$work/icon.ico" bsw/assets/icon.png
makensis -V2 -DVERSION="$version" -DAPP="/src/$app" -DICON="/src/$work/icon.ico" \
    -DOUTFILE="/src/target/package/arctos-recorder-$version-setup.exe" deploy/windows/installer.nsi
