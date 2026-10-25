# volume-control

- **类型**：程序（可执行文件）
- **作用**：音量控制
- **产物 / 安装位置**：`/bin/sndvol`
- **对应的 Windows 2000 组件**：sndvol32.exe
- **依赖**：sndvol、classic-ui、classic-theme、canvas、typeface

单列混音器窗口（菜单栏、平衡与音量滑块、全部静音），调整 ALSA Master 的左右音量与静音；状态与布局来自库 `sndvol`，托盘小面板复用同一份。
