# Desktop runtime

## Process topology

```text
BusyBox init
  └─ liteui-session
       ├─ display-session
       ├─ liteui-compositor ── liteui-core
       ├─ liteui-host ──────── System Shell APK
       ├─ liteui-host ──────── application APK
       └─ terminal-service ─── PTY / ANSI model
```

`liteui-session` 管生命周期和 authority；`liteui-compositor` 管可见桌面事实；`liteui-host`
只解释一个应用。这个分割使 JS OOM、Shell crash 和 compositor crash 分别拥有不同 recovery
domain，而不是靠一个万能进程内的异常分支猜测恢复方式。

应用 APK 永远以 `app.mjs` + typed style/assets 为权威输入。host 只把 QuickJS build ID、
LiteUI ABI、compiler options 与 APK bundle digest 完全匹配的 bytecode 交给 QuickJS reader；cache
校验失败便删除并从 source 重建，以临时文件 + fsync + rename 发布。QuickJS bytecode reader 本身
不验证输入，因此 cache 目录只属于固定 `liteui` identity，JS HostOps 不暴露任意文件 I/O。

## Boot without login

1. init 只启动 session 与 UART recovery shell。
2. session 先创建 activated seat/compositor listener，再启动 display-session、compositor、System
   Shell host、Calculator host 与 terminal service；每一步失败都逆序清理已发布成员。
3. compositor 取得 display capability并 modeset 后显示固定 recovery scene；它不是第二套产品 UI，只是 QuickJS failure
   时保证屏幕可诊断的 fail-safe。
4. session 以固定 uid 100 启动 System Shell APK。Shell 的首个 root transaction 完整
   验证后，在 frame boundary 原子替换 recovery root。
5. compositor 从 Shell 的 TextGrid viewport 产生 configure；uid 101 terminal service 据此创建 PTY，
   uid 102 Calculator host 独立发布自己的 subtree。
6. 第一阶段直接进入 desktop；未来 login 是独立 APK/Runtime，成功后必须终止 pre-login Runtime
   再创建 user session，禁止跨 authentication boundary 复用 heap/capability。

## Failure domains

- 应用退出：撤销该应用 subtree、window 与 resource；其他 client 不变。
- Shell 退出：compositor 保持普通窗口并显示最小 recovery chrome；session 重启 Shell。
- compositor 退出：session 终止所有旧 client/service，随后创建全新的进程 generation。
- display-session 无法可靠 revoke：退出当前 session；连续失败留在 UART recovery。

所有 restart 都由 event/wait 驱动，不使用固定 sleep。idle session 没有 periodic connector poll 或
固定 60 Hz wakeup。
