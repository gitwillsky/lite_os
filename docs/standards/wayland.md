# Wayland 与拼音输入基线

- Wayland `1.24.0`；tag commit `736d12ac67c20c60dc406dc49bb06be878501f86`；`protocol/wayland.xml` SHA-256
  `60abb5864546288a660f3d8af0c838fc87c85bb386582da23e52cb8476e9adbf`。
- wayland-protocols `1.45`；tag commit `0091197f5c1b1f2c131f1410e99f9c95d50646be`；`stable/xdg-shell/xdg-shell.xml` SHA-256
  `9d9e27038ca722f159a361293f8c6cd270dab907ab52de5b8ecfbb8d6b9651c2`，`stable/viewporter/viewporter.xml` SHA-256
  `dcb12279a03746301fe490aaed4b38a403485a925abfce2ccfceb644e104fe71`。
- 同一 wayland-protocols `1.45` 提交的 `staging/security-context/security-context-v1.xml`（v1）SHA-256
  `f4f8ba15ebd2df62db611b1de32e992f660a4aeeea83250cb88fa468da42ffcb`；XML 原样 vendored，保留上游测试阶段声明。
- 同一 wayland-protocols `1.45` 提交的 `staging/cursor-shape/cursor-shape-v1.xml`（v2）SHA-256
  `bb57d91e53a79dadab7c612dab87c233393cee73673feefa7442cfbfdd9aed2f`，以及它引用 `zwp_tablet_tool_v2` 所需的
  `stable/tablet/tablet-v2.xml` SHA-256 `3ea2783210086dfddef3994fec974f46a00cf175c06efdd8ea47c3007f812a80`；XML 原样 vendored，
  compositor 只公布 cursor-shape，不公布平板全局对象，请求 `get_tablet_tool_v2` 是协议错误。
- 同一 wayland-protocols `1.45` 提交的 `unstable/xdg-output/xdg-output-unstable-v1.xml`（v3）SHA-256
  `363d547c3eefe8959160cac903ff90b311d4b183005557d73640d9df2cfd7f79`；XML 原样 vendored。
- 同一 wayland-protocols `1.45` 提交的 `staging/xdg-dialog/xdg-dialog-v1.xml`（v1）SHA-256
  `2822956bdf13d51848ff4b5e29c509028c1204c6e06262ee926b001f43a42dbb`；XML 原样 vendored。客户端自行筛选父窗口输入，合成器消费模态提示辅助焦点策略。
- 输入法使用 wayland-protocols 1.45 同一提交的 `unstable/text-input/text-input-unstable-v3.xml`，
  SHA-256 `2d08f2cddb463e169c23f1c34769de12ae255540e51ee8f515b54667d60b90ba`。
  [wlroots 固定提交](https://github.com/swaywm/wlroots/tree/0855cdacb2eeeff35849e2e9c4db0aa996d78d10/protocol) 的
  `input-method-unstable-v2.xml` SHA-256 `99414dbad9458e71aa1fa01bc45f94ca6685787bfcb4d98948f72c1b45b60703`，
  `virtual-keyboard-unstable-v1.xml` SHA-256 `7ad7870003ecd592cae47dc19d277a609b7f18fd7b7be012623cf3225a7294f5`。
  转发未消费的键与修饰位参照 [Fcitx5 5.1.13 的 Wayland v2 frontend](https://github.com/fcitx/fcitx5/blob/0f26cf3262338e62b7a56c0d51e2b17b60e6315f/src/frontend/waylandim/waylandimserverv2.cpp)。
- 拼音词表由 `scripts/generate_pinyin_dictionary.py` 从固定读音和频率输入可重复生成。源版本与授权见
  [词表声明](../../assets/pinyin/pinyin-NOTICE.md)。按键选择、单独 Shift 模式切换和预编辑行为参照
  [Microsoft 拼音](https://support.microsoft.com/zh-cn/windows/hardware/input-devices/microsoft-simplified-chinese-ime)。
- wlr-protocols commit `bf4fc79abc359eea5a0edec0ac6d4a2b2955f82a`：`unstable/wlr-layer-shell-unstable-v1.xml`（v5）SHA-256
  `87e0b9c837aecd6977f76f3c47d73088b7159871f5d979dc1840f6cadb5e2ed8`；
  `unstable/wlr-foreign-toplevel-management-unstable-v1.xml`（v3）SHA-256
  `4ecc4588858e29fe680a33521e1f22bcf22071d66d1553fe96b4ddec03d591d2`。
  来源：[wlr-protocols 固定提交](https://gitlab.freedesktop.org/wlroots/wlr-protocols/-/tree/bf4fc79abc359eea5a0edec0ac6d4a2b2955f82a)。
- 来源：[Wayland](https://gitlab.freedesktop.org/wayland/wayland/-/tree/1.24.0)、
  [wayland-protocols](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/tree/1.45)。
- 运行时一致性消费者：Alpine v3.22 的 `wayland-utils-1.2.0-r0`（`wayland-info`）、`wayland-libs-client-1.23.1-r3`、
  `libffi-3.4.8-r0`、`libdrm-2.4.124-r0`、`libpciaccess-0.18.1-r0`，以及裁决 keymap 的 `libxkbcommon-1.8.1-r2`（含 `-dev` 头文件）、
  `libxml2-2.13.9-r1`、`zlib-1.3.2-r1`、`xz-libs-5.8.4-r0`，SHA-256 固定在 `scripts/verify_wayland.py`。keymap 文本格式取自 libxkbcommon 的 `XKB_KEYMAP_FORMAT_TEXT_V1`。
  XML 原样放入 `user/libraries/wayland-wire/protocols/`，由 `wayland-scanner` 生成类型；wire 格式以 [Wayland wire 规范](https://wayland.freedesktop.org/docs/book/Protocol.html#wire-format)为准。
  消息头长度为 16-bit、参数四字节对齐；单次读写缓冲大小不能作为协议消息上限，字符串不允许内部 NUL。
