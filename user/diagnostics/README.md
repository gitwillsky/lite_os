# user/diagnostics — 诊断程序

C 编写的 `liteos-stress.c`：cputest、memtest、cachetest 的有界 multicall 诊断，只用于验证，不是桌面产品的一部分。

`liteos-driver-update.c` 由主机诊断工具按需交叉编译，通过 AF_UNIX 递交已打开的原生映像。
使用方式见[运行时更新驱动或驱动管理器](../../docs/development/build-and-verify.md#运行时更新驱动或驱动管理器)。
