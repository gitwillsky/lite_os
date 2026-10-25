# control-panel

- **类型**：程序（可执行文件）
- **作用**：控制面板小程序
- **产物 / 安装位置**：`/bin/control-panel <system|datetime|sounds|display|taskbar|winver>`
- **对应的 Windows 2000 组件**：*.cpl 小程序与 winver.exe
- **依赖**：classic-ui、classic-theme、canvas、typeface、calendar、sound

系统属性、日期/时间属性与"关于"是只读信息页；"声音和多媒体"有"声音"（事件列表与试听）与"音频"（播放设备与音量控制入口）两页。"显示"（背景色）与"任务栏"（总在最前、自动隐藏、显示时钟）写入每用户的 `desktop.conf`（库 `desktop-settings`），桌面外壳按修改时间发现并立即应用。设置程序通过 `Categories=Settings` 的桌面条目出现在控制面板里。
