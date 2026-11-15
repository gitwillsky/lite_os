# pinyin-ime

独立用户态程序。它拥有内置简体全拼的组词、只读词表索引、候选窗、键盘选择和任务栏「中／英」状态。
登录会话启动并授权该进程，Wayland 合成器负责投递与定位。应用使用标准 text-input 协议取得上屏文本。

[会话授权与文本输入契约](../../../docs/architecture-contract/text-input.md) ·
[词表来源与许可](../../../assets/pinyin/pinyin-NOTICE.md)
