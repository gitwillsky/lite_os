#!/bin/sh
# 在 Alpine 3.24 aarch64 虚拟机（musl）内构建 Wine 11.16：aarch64 宿主 + i386 PE（WoW64，经 FEX）。
# 详见 docs/development/wine-runtime.md。参数：补丁目录（默认 third_party/wine/patches，需先拷进 guest）。
# 产物安装到 /opt/wine；之后运行 scripts/wine/package-wine.sh 生成可提交的运行时压缩包。
set -eu
VER=11.16
SHA256=c66e2090343dcd727f7f7fd2f87ee0bfb0b118790c1d745ab7b8a4c3a4197f2f
PATCHES=${1:-$HOME/wine-patches}
doas apk add -q clang lld llvm bison flex-dev autoconf automake perl \
  alsa-lib-dev wayland-dev wayland-protocols libxkbcommon-dev freetype-dev fontconfig-dev \
  gnutls-dev ncurses-dev linux-headers gettext
mkdir -p ~/wine-build && cd ~/wine-build
[ -f wine-$VER.tar.xz ] || curl -sSfLO https://dl.winehq.org/wine/source/11.x/wine-$VER.tar.xz
echo "$SHA256  wine-$VER.tar.xz" | sha256sum -c -
# 补丁：01-08 是 Hangover 11.16 的 8 个提交（FEX 的 WoW64 DLL 依赖其中的 32 位地址空间隔离与线程挂起接口），
# rpath.patch 来自 Alpine（musl 不按 soname 复用已加载的 ntdll.so，unix 侧 .so 必须带 $ORIGIN rpath）。
[ -d wine-$VER ] || { tar xf wine-$VER.tar.xz &&
  patch -d wine-$VER -p1 < "$PATCHES/rpath.patch" &&
  for p in "$PATCHES"/0*.patch; do patch -d wine-$VER -p1 < "$p"; done; }
mkdir -p build && cd build
CC=clang CXX=clang++ LDFLAGS="-fuse-ld=lld" ../wine-$VER/configure \
  --prefix=/opt/wine --enable-archs=aarch64,i386 --with-mingw=clang \
  --without-x --with-wayland --with-alsa --without-pulse --without-gstreamer \
  --without-cups --without-sane --without-gphoto --without-pcap --without-usb \
  --without-v4l2 --without-opencl --without-oss --without-dbus --without-udev \
  --without-vulkan --without-opengl --without-sdl --without-capi --without-krb5 --without-netapi \
  > configure.log 2>&1 || { tail -40 configure.log; exit 1; }
make -j"$(nproc)" > make.log 2>&1 || { tail -60 make.log; exit 1; }
doas make install > install.log 2>&1
echo BUILD_DONE
