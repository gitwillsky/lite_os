#!/bin/sh
# 把 /opt/wine 裁成运行时树并打包：只保留 wine、wineserver 与常用小程序、三套库目录与 share/wine；
# 去掉头文件、静态库、man、开发工具与调试信息。产物 wine-<版本>-aarch64-musl.tar.xz 提交在 prebuilt/wine/。
set -eu
OUT=${1:-/tmp/wine-rt}
rm -rf "$OUT"; mkdir -p "$OUT/bin" "$OUT/lib/wine" "$OUT/share"
cd /opt/wine
for b in wine wineserver wineboot winecfg notepad winemine regedit regsvr32 msiexec winepath; do
  cp "bin/$b" "$OUT/bin/"
done
# 不用 cp -a：非 root 无法保留属主。
cp -R lib/wine/aarch64-unix lib/wine/aarch64-windows lib/wine/i386-windows "$OUT/lib/wine/"
cp -R share/wine "$OUT/share/"
cd "$OUT"
find . -name '*.so' -exec strip --strip-unneeded {} + >/dev/null 2>&1 || true
find lib/wine/aarch64-windows lib/wine/i386-windows -type f -exec llvm-strip --strip-debug {} + >/dev/null 2>&1 || true
find lib/wine -name '*.a' -delete
cd "$(dirname "$OUT")"
tar -cf - "$(basename "$OUT")" | xz -9 -T"$(nproc)" > "$(basename "$OUT").tar.xz"
sha256sum "$(basename "$OUT").tar.xz"
