# The image that builds the base station's Ubuntu package; `just
# package-linux` runs deploy/linux/package.sh in it with the repository
# mounted at /src.
#
# It is the oldest Ubuntu the package supports: the package links against
# this release's system libraries, which later releases keep providing.

FROM ubuntu:22.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang cmake curl dpkg-dev file libclang-dev \
        libgtk-3-dev libssl-dev libwebkit2gtk-4.1-dev libxdo-dev nasm pkg-config xz-utils \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_VERSION=1.92.0
ENV RUSTUP_HOME=/opt/rustup
RUN curl -sSf https://sh.rustup.rs \
        | CARGO_HOME=/opt/cargo sh -s -- -y --no-modify-path --profile minimal --default-toolchain $RUST_VERSION
ENV PATH=/opt/rustup/toolchains/$RUST_VERSION-x86_64-unknown-linux-gnu/bin:$PATH

COPY deploy/opencv.sh /tmp/
RUN /tmp/opencv.sh /opt/opencv && rm /tmp/opencv.sh
ENV OPENCV_INCLUDE_PATHS=/opt/opencv/include/opencv4 \
    OPENCV_LINK_PATHS=/opt/opencv/lib,/opt/opencv/lib/opencv4/3rdparty \
    OPENCV_LINK_LIBS=static=opencv_calib3d,static=opencv_features2d,static=opencv_flann,static=opencv_imgproc,static=opencv_core,static=zlib

# ffmpeg 8.1 rather than 9: NVENC in 9 needs NVIDIA's 610 drivers or later.
ARG FFMPEG_RELEASE=autobuild-2026-07-31-14-10
ARG FFMPEG=ffmpeg-n8.1.2-34-g9b6c8969e0-linux64-gpl-8.1
ARG FFMPEG_SHA256=09fc77be269c7053e438b7e96548e4af97604faf96a42c4a3c56a1ad74c22c0a
RUN curl -sSfL -o /tmp/ffmpeg.tar.xz \
        https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_RELEASE/$FFMPEG.tar.xz \
    && echo "$FFMPEG_SHA256  /tmp/ffmpeg.tar.xz" | sha256sum -c \
    && mkdir /opt/ffmpeg \
    && tar -xJf /tmp/ffmpeg.tar.xz -C /opt/ffmpeg --strip-components 2 $FFMPEG/bin/ffmpeg \
    && rm /tmp/ffmpeg.tar.xz
