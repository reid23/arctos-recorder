# The cloud stitching server. Build from the repository root:
#
#   docker build -f ssw/Dockerfile -t arctos-recorder-ssw .
#
# and run on a host with the NVIDIA Container Toolkit:
#
#   docker run --gpus all -p 7800:7800 -e SSW_TOKEN=… arctos-recorder-ssw
#
# Nothing it stores needs to outlive the container: base stations send
# again any job the server doesn't know about.
#
# Without a GPU it falls back to software Vulkan and x264, which works but is
# far too slow for real use.

FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang cmake nasm pkg-config \
        libavcodec-dev libavdevice-dev libavfilter-dev libavformat-dev \
        libavutil-dev libswresample-dev libswscale-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release -p ssw && cp target/release/ssw /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates ffmpeg libvulkan1 mesa-vulkan-drivers \
        chromium fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ssw /usr/local/bin/
# Graphics for Vulkan stitching, video for NVENC/NVDEC. Chromium (for the
# live scoreboard overlay) runs as root, which its sandbox doesn't allow.
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=all \
    SSW_DATA_DIR=/data \
    CHROMIUM_FLAGS=--no-sandbox
EXPOSE 7800
ENTRYPOINT ["ssw"]
