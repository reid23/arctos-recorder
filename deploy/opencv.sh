#!/bin/sh
# Builds the parts of OpenCV the calibration crate uses, as static libraries,
# and installs them into the folder given:
#
#   deploy/opencv.sh PREFIX [CMAKE_ARGUMENTS...]
#
# e.g. a toolchain file when cross-compiling. Every platform gets the same
# layout and library names, so the opencv crate links them with:
#
#   OPENCV_INCLUDE_PATHS=PREFIX/include/opencv4
#   OPENCV_LINK_PATHS=PREFIX/lib,PREFIX/lib/opencv4/3rdparty
#   OPENCV_LINK_LIBS=static=opencv_calib3d,static=opencv_features2d,static=opencv_flann,static=opencv_imgproc,static=opencv_core,static=zlib
set -eu

version=4.12.0
prefix=$1
shift

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -sSfL "https://github.com/opencv/opencv/archive/refs/tags/$version.tar.gz" | tar -xz -C "$work"
cmake -S "$work/opencv-$version" -B "$work/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DOPENCV_INCLUDE_INSTALL_PATH=include/opencv4 \
    -DOPENCV_LIB_INSTALL_PATH=lib \
    -DOPENCV_3P_LIB_INSTALL_PATH=lib/opencv4/3rdparty \
    -DOPENCV_LIB_ARCHIVE_INSTALL_PATH=lib \
    -DOPENCV_CONFIG_INSTALL_PATH=lib/cmake/opencv4 \
    -DOPENCV_DLLVERSION= \
    -DBUILD_SHARED_LIBS=OFF \
    -DBUILD_LIST=calib3d,features2d,flann,imgproc,core \
    -DBUILD_ZLIB=ON \
    -DBUILD_TESTS=OFF -DBUILD_PERF_TESTS=OFF -DBUILD_EXAMPLES=OFF -DBUILD_DOCS=OFF \
    -DBUILD_opencv_apps=OFF -DBUILD_JAVA=OFF -DBUILD_opencv_python3=OFF \
    -DWITH_IPP=OFF -DWITH_ITT=OFF -DWITH_OPENCL=OFF -DWITH_LAPACK=OFF -DWITH_EIGEN=OFF \
    -DWITH_OPENMP=OFF -DWITH_TBB=OFF -DWITH_PROTOBUF=OFF -DWITH_ADE=OFF \
    -DWITH_KLEIDICV=OFF -DWITH_CAROTENE=OFF \
    -DWITH_JPEG=OFF -DWITH_PNG=OFF -DWITH_TIFF=OFF -DWITH_WEBP=OFF -DWITH_OPENJPEG=OFF \
    -DWITH_JASPER=OFF -DWITH_OPENEXR=OFF -DWITH_AVIF=OFF -DWITH_FFMPEG=OFF -DWITH_GSTREAMER=OFF \
    -DWITH_V4L=OFF -DWITH_MSMF=OFF -DWITH_DSHOW=OFF -DWITH_AVFOUNDATION=OFF -DWITH_GTK=OFF \
    -DWITH_QT=OFF -DWITH_WIN32UI=OFF -DWITH_VTK=OFF \
    -DOPENCV_GENERATE_PKGCONFIG=OFF \
    "$@"
cmake --build "$work/build" --parallel
cmake --install "$work/build"
