# Phase 32：musl content-addressed build cache

## 问题与基线

`verify_musl.py --build-only` 原本每次删除 source/build/install，导致已缓存 tarball 仍必须重新解压、configure、全量编译与 install。优化前热运行实测 `29.65s`；分段计时中 source 获取 `1.486s`、musl build/install `20.401s`、smoke link `0.152s`、ELF 检查 `0.010s`。

## 缓存契约

1. source fingerprint 包含固定 musl version/revision、官方 archive SHA-256 和 extraction recipe。
2. sysroot fingerprint 包含 source fingerprint、compiler realpath/target/version、configure arguments、sanitized environment 和显式 recipe version。
3. smoke fingerprint 包含 sysroot fingerprint、consumer SHA-256、link arguments、compiler identity 和 libgcc SHA-256。

命中必须同时满足 manifest 完全匹配与必要 artifact 存在；不使用 mtime 或“`libc.a` 存在”伪判定。consumer 变化只失效 smoke，不失效 sysroot。

## 发布与并发

同一 cache 的 writer 由 `flock` 串行化。sysroot/smoke 先构建到新的不可变 generation，必要 artifact 和 manifest 完整后再通过 `os.replace` 原子切换 fingerprint symlink。已运行的 consumer 持有 resolved generation path，显式 `--rebuild` 不会使其路径失效。旧 generation 只由 `make clean-musl` 清理。

## 并行度

不再固定上限 8。上层 GNU Make jobserver 存在时直接继承；独立运行默认使用宿主 `os.cpu_count()`；`LITEOS_BUILD_JOBS` 只接受正整数并显式覆盖。并行度不影响输出 fingerprint。

## 验收证据

- 新 cache 冷构建：`18.24s`。
- 同 fingerprint 热构建：`0.15s` 至 `0.18s`，相对旧基线至少约 165 倍加速。
- 后台 `--rebuild` 时前台持续校验 manifest/`libc.a`：`probe_rc=0`、`build_rc=0`、pointer 已切换。
- BusyBox builder 通过受验 manifest 查询取得 resolved source/sysroot，不依赖旧固定 install path。
- cache 命中后仍每次执行 smoke ELF 检查；非 `--build-only` 路径仍构造 ext2 并冷启动 QEMU。
