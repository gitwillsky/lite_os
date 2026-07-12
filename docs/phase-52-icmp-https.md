# Phase 52：ICMP 与证书校验 HTTPS 竖切

本阶段固定对照 Linux v7.1 raw IPv4/pselect6/riscv_hwprobe ABI、BusyBox 1.37.0、OpenSSL 3.5.7 LTS 与 Mozilla CA extract 2026-05-14，在唯一 `VirtIO-net → NetworkStack → Socket/OFD → syscall → musl/BusyBox/OpenSSL` 路径上完成 `ping` 和 HTTPS。

## 实现边界

- effective UID 0 可创建 `AF_INET/SOCK_RAW/IPPROTO_ICMP`；smoltcp raw endpoint 属于唯一 NetworkStack，发送时由 kernel 补 IPv4 header，接收返回 IPv4 header 与 ICMP payload。未开放其他 raw IP protocol。
- `FIONBIO` 更新 OFD 唯一 status flags；`TCP_NODELAY` 直接控制 smoltcp Nagle policy；`pselect6` 与 ppoll 共用 IndexedWaitQueue，不存在 OpenSSL 专用等待路径。
- `riscv_hwprobe` 只经 system façade 投影 DTB/HartTopology 平台事实；当前完整支持 flags=0 value query，`WHICH_CPUS` 精确记录为未开放。
- BusyBox 只启用 `FEATURE_WGET_OPENSSL`，明确关闭不验证证书的 internal TLS。发布 rootfs 固定 OpenSSL LTS 与 Mozilla CA bundle；临时 gate CA 只注入 disposable runtime image。

## 运行验收事实

- QEMU slirp gateway 收到 BusyBox `ping -c 1` Echo Reply。
- 受控 loopback HTTPS origin 使用临时 CA 与 DNS SAN：正确 hostname 下载固定 payload，直接以 gateway IP 访问必须因 hostname/IP mismatch 失败。
- OpenSSL 实际消费 `riscv_hwprobe`、`FIONBIO`、`TCP_NODELAY`、`pselect6`、getrandom、realtime、DNS 与现有 TCP stream；禁止 `--no-check-certificate` 或明文 redirect 作为验收。
