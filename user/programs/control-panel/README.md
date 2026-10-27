# control-panel

类型：程序，产物 `/bin/control-panel <system|datetime|sounds|display|taskbar|network|properties|winver>`。

各小程序提供业务数据、控件声明、验证和后台应用闭包；共享 PropertySheet 拥有标签页、OK/Cancel/Apply 与错误定位。
文件统计、网络采样、设置持久化和时钟/时区修改均在后台执行。
显示与任务栏只合并自身拥有的设置字段；desktop-settings 以标准文件锁保护读改写并原子发布。
“显示 > 背景”按 Windows 2000 shell32 背景页：系统墙纸（`/usr/share/liteos/wallpapers`，ISO 原安装名）与“浏览(B)...”选择的 BMP/JPEG/PNG、
显示图片（居中/平铺/拉伸）、背景颜色与监视器预览；不提供 HTML 墙纸和图案。
依赖 liteos-ui、calendar、desktop-settings 和 linux-uapi，不含标准控件绘制或像素提交。
