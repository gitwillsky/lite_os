# Phase 33：BusyBox content-addressed build cache

## 问题与基线

`verify_busybox.py --build-only --image fs.img` 原本每次删除源码树，导致相同输入仍重复解压、配置和全量编译。优化前热运行实测 `29.38s`；命令级计时中解压 `2.138s`、`allnoconfig` `7.383s`、编译链接 `12.044s`，ELF 与 ext2/debugfs 检查合计不足 `0.3s`。

## 缓存契约

1. source fingerprint 包含固定 BusyBox version、官方 archive SHA-256 与 extraction recipe。
2. binary fingerprint 包含 source fingerprint、唯一 config fragment SHA-256、musl sysroot fingerprint、compiler identity、目标架构、受控环境与 specs 适配 recipe。
3. 命中必须同时满足 manifest 完全匹配和 BusyBox ELF 存在；mtime、并行度和输出 image 路径不参与 fingerprint。

source 作为不可变输入，configure/build 使用 BusyBox 原生 `O=<output>` 在私有输出树执行。最终 ELF 写入不可变 generation，manifest 就绪后通过原子 symlink 发布。旧 generation 只由 `make clean-busybox` 清理。

## 镜像边界

ext2 image 不作为 binary cache 的输入或共享输出。每次调用都从已校验 ELF 重新创建用户指定的 image、写入唯一 inittab 与 hardlink applets，并重新检查目录项、inode 一致性和 link count。这样既消除昂贵的重复编译，也不让上次运行产生的可写文件系统状态污染本次验收。

## 验收证据

- 优化前相同输入热运行：`29.38s`。
- 首次填充新 cache 并完整构建：`23.05s`。
- 同 fingerprint 热运行：`0.33s` 至 `0.52s`，至少约 56 倍加速。
- 后台 `--rebuild` 时前台持续校验 manifest/ELF：`probe_rc=0`、`build_rc=0`、pointer 已切换。
- cache 命中后仍执行 BusyBox ELF 检查，并重新创建、检查指定 ext2 image。
- artifact gate 通过同一 manifest API 解析当前 generation，不再读取旧 `source/busybox` 路径。
- musl 与 BusyBox 共用 `build_cache.py` 中的 manifest、锁、临时目录、generation 和原子发布实现，不保留两套 cache primitive。
