# The image that cross-compiles the base station for Windows and builds its
# installer; `just package-windows` runs deploy/windows/package.sh in it
# with the repository mounted at /src.

FROM debian:trixie
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates clang cmake curl g++-mingw-w64-x86-64-posix icoutils libclang-dev make \
        nasm nsis unzip \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_VERSION=1.92.0
ENV RUSTUP_HOME=/opt/rustup
RUN curl -sSf https://sh.rustup.rs \
        | CARGO_HOME=/opt/cargo sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain $RUST_VERSION --target x86_64-pc-windows-gnu
ENV PATH=/opt/rustup/toolchains/$RUST_VERSION-x86_64-unknown-linux-gnu/bin:$PATH

COPY deploy/opencv.sh deploy/windows/mingw.cmake /opt/
RUN /opt/opencv.sh /opt/opencv -DCMAKE_TOOLCHAIN_FILE=/opt/mingw.cmake
# The opencv crate parses OpenCV's headers with clang, which has to read
# them as Windows does (where a `long` is 32 bits, for one), with MinGW's
# C++ library rather than this system's.
RUN printf '#!/bin/sh\nexec clang --target=x86_64-w64-mingw32 "$@"\n' > /opt/mingw-clang \
    && chmod +x /opt/mingw-clang
ENV OPENCV_INCLUDE_PATHS=/opt/opencv/include/opencv4 \
    OPENCV_LINK_PATHS=/opt/opencv/lib,/opt/opencv/lib/opencv4/3rdparty \
    OPENCV_LINK_LIBS=static=opencv_calib3d,static=opencv_features2d,static=opencv_flann,static=opencv_imgproc,static=opencv_core,static=zlib \
    OPENCV_CLANG_ARGS=--target=x86_64-w64-mingw32 \
    CLANG_PATH=/opt/mingw-clang \
    CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix \
    CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++-posix \
    AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
    CMAKE_TOOLCHAIN_FILE_x86_64_pc_windows_gnu=/opt/mingw.cmake \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-posix

# ffmpeg 8.1 rather than 9: NVENC in 9 needs NVIDIA's 610 drivers or later.
ARG FFMPEG_RELEASE=autobuild-2026-07-31-14-10
ARG FFMPEG=ffmpeg-n8.1.2-34-g9b6c8969e0-win64-gpl-8.1
ARG FFMPEG_SHA256=cc4156d51387566ea8ba653fc3a04897bdf812fddf652428d9030bbf7ae24835
RUN curl -sSfL -o /tmp/ffmpeg.zip \
        https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_RELEASE/$FFMPEG.zip \
    && echo "$FFMPEG_SHA256  /tmp/ffmpeg.zip" | sha256sum -c \
    && unzip -j /tmp/ffmpeg.zip $FFMPEG/bin/ffmpeg.exe $FFMPEG/LICENSE.txt -d /opt/ffmpeg \
    && rm /tmp/ffmpeg.zip
