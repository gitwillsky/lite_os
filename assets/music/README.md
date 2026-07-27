# LiteOS 内置音乐

`跟太阳系说再见/二向箔降维打击.flac` 是用户提供的免费音乐，用作 LiteOS Music Player
的预装可播放内容。rootfs 构建将它安装到
`/root/Music/跟太阳系说再见-二向箔降维打击.flac`。

音乐文件使用 Git LFS 保存。普通 `sync-userland` 不管理 `/root/Music`，因此不会覆盖用户后来
添加的歌曲；`make reset-rootfs` 会从仓库资产恢复这份预装音乐。

`cover.png` 是为该曲生成的 LiteOS Music Player 专辑封面；UI 构建将其作为
`music-player/assets/solar-system-cover.png` 发布。它不写入 `/root/Music`，避免把应用呈现资产
误当作用户音乐文件。
