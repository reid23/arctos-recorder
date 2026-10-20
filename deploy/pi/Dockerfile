# The image that cross-compiles the camera software into a Debian package
# and writes the cameras' Raspberry Pi OS image; `just package-csw` and
# `just pi-image` run deploy/pi/package.sh and deploy/pi/image.sh in it
# with the repository mounted at /src.
#
# libcamera is the very build in the camera image (see deploy/pi/image.sh):
# Raspberry Pi changes its ABI without changing its soname, so the camera
# software only runs against the libcamera it was built with. The other
# libraries come from Debian, since Raspberry Pi's builds of Debian's
# libraries can't be installed alongside this system's own.

FROM debian:trixie
ARG KEYRING=raspberrypi-archive-keyring_2025.1+rpt1_all.deb
ARG KEYRING_SHA256=2e727149d7acb8cc7f604e66d0049161039c8aa1eaf1175e54f9e69d963d60e4
ARG LIBCAMERA=0.7.2+rpt20260817-1
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -sSfL -o /tmp/keyring.deb \
        https://archive.raspberrypi.com/debian/pool/main/r/raspberrypi-archive-keyring/$KEYRING \
    && echo "$KEYRING_SHA256  /tmp/keyring.deb" | sha256sum -c \
    && dpkg-deb -x /tmp/keyring.deb /tmp/keyring \
    && cp /tmp/keyring/usr/share/keyrings/raspberrypi-archive-keyring.pgp /usr/share/keyrings/ \
    && rm -r /tmp/keyring.deb /tmp/keyring \
    && echo "deb [arch=arm64 signed-by=/usr/share/keyrings/raspberrypi-archive-keyring.pgp] http://archive.raspberrypi.com/debian trixie main" \
        > /etc/apt/sources.list.d/raspberrypi.list \
    && printf 'Package: libpisp* libpisp*:arm64\nPin: origin archive.raspberrypi.com\nPin-Priority: 600\n\nPackage: *\nPin: origin archive.raspberrypi.com\nPin-Priority: 100\n' \
        > /etc/apt/preferences.d/raspberrypi \
    && dpkg --add-architecture arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
        clang cmake crossbuild-essential-arm64 dpkg-dev e2fsprogs fdisk libclang-dev mtools openssl \
        pkg-config xz-utils libasound2-dev:arm64 \
    && cd /tmp \
    && for package in libcamera-dev libcamera0.7 libcamera-ipa; do \
        curl -sSfLO https://archive.raspberrypi.com/debian/pool/main/libc/libcamera/${package}_${LIBCAMERA}_arm64.deb; \
    done \
    && printf '%s\n' \
        "b799340d3f753563f583f705af26411b45d8426b6f13d0b734b4da0872bb3b62  libcamera-dev_${LIBCAMERA}_arm64.deb" \
        "c7dfaadcf9cd3b03fc41f0d457c88736d2638fa79d1cca11fc3637777cdae3c9  libcamera0.7_${LIBCAMERA}_arm64.deb" \
        "0de238bc7f0a23bf4ab90604d232c88e92c0b5fa724272d555fdc570ca4d30f4  libcamera-ipa_${LIBCAMERA}_arm64.deb" \
        | sha256sum -c \
    && apt-get install -y --no-install-recommends ./libcamera-dev_*.deb ./libcamera0.7_*.deb ./libcamera-ipa_*.deb \
    && rm -rf /tmp/*.deb /var/lib/apt/lists/*

ARG RUST_VERSION=1.92.0
ENV RUSTUP_HOME=/opt/rustup
RUN curl -sSf https://sh.rustup.rs \
        | CARGO_HOME=/opt/cargo sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain $RUST_VERSION --target aarch64-unknown-linux-gnu
ENV PATH=/opt/rustup/toolchains/$RUST_VERSION-x86_64-unknown-linux-gnu/bin:$PATH

COPY deploy/pi/aarch64.cmake /opt/
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CMAKE_TOOLCHAIN_FILE_aarch64_unknown_linux_gnu=/opt/aarch64.cmake \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig
