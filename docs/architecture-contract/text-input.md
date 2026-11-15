# Wayland 会话授权与文本输入契约

- foreign-toplevel 的 title/app_id/state 只由 Display 的 done/closed 更新。SDK token 与协议对象代际一起校验，
  不复制协议状态或把可复用 wire id 当作长期应用标识。列表选择由运行库保存，应用按需读取稳定条目标识。
  同一桌面的普通应用可使用标准 foreign-toplevel，任务管理器依赖该能力；完整的 layer-shell 授予 Session/Shell，
  security-context manager 只授予 Session。公开 socket 的 root 身份也不能获得后两者。
- 认证后 `logon` 用独立子进程组启动 `pinyin-ime`，由受信任的 Session 经 security-context
  委派唯一 InputMethod 连接。普通 socket 与 Shell 均不能绑定 grab 或虚拟键盘。InputMethod 的 layer-shell
  仅允许一个固定大小、无键盘焦点的状态指示器，任务栏不可见时随之隐藏。候选用 input-popup 角色。
- seat 拥有物理焦点和抓取按下归属。Wayland relay 只连接当前获得键盘焦点且已 enable 的输入对象，
  在字段撤销、换焦点、登录锁定与连接退出时退休旧路由和未释放的转发键。敏感字段不传上下文。
  输入法进程拥有词库、组词、模式和状态图标像素。合成器只定位、合成并路由协议事件。
- 标准编辑控件的预编辑随焦点持有，正文只在完整 done 之后修改。Canvas 以局部矩形显式声明文本光标，
  ABI 表尾的 caret 与 commit 读接口保持 v1 事件布局。commit 字符串在事件交付时完整读取；
  终端仅把上屏字符串送既有 PTY 队列。词表索引启动时建立，输入查询有界，随桌面门禁验证。

固定 XML、兼容消费者与字典来源见[Wayland 工程基线](../standards/wayland.md)。
