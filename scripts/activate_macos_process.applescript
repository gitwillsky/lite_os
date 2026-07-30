(*
等待指定 PID 注册为 Cocoa application，然后将其切到前台。

参数：即将由 exec 保持不变的进程 PID，以及初始窗口外框宽、高。
返回：目标窗口采用初始 Retina 逻辑尺寸并成为 frontmost 后正常退出。
错误：参数非法或进程未在期限内注册时抛出。
*)
on run arguments
    -- 1. PID 是 Shell 与 exec 后 QEMU 共享的 identity；缺失时可能错误激活无关应用。
    if (count of arguments) is not 3 then error "expected process ID, window width and height"
    set processId to item 1 of arguments as integer
    if processId is less than 1 then error "expected a positive process ID"
    set windowWidth to item 2 of arguments as integer
    set windowHeight to item 3 of arguments as integer
    if windowWidth is less than 1 or windowHeight is less than 1 then error "expected a positive window size"

    tell application "System Events"
        -- 2. QEMU 在 Cocoa 初始化后才注册 application process；立即查询会稳定得到空结果。
        repeat 60 times
            if exists first application process whose unix id is processId then
                set targetProcess to first application process whose unix id is processId
                if exists window 1 of targetProcess then
                    -- 3. zoom-to-fit 允许窗口驱动 guest mode；先移到可用屏幕原点再设置外框，
                    --    避免居中的 QEMU 默认小窗因右/下屏幕边界把初始尺寸钳小。
                    set position of window 1 of targetProcess to {0, 28}
                    set size of window 1 of targetProcess to {windowWidth, windowHeight}
                    -- 4. QEMU CLI 的 zoom-to-fit=on 会把临时 640×360 Cocoa bootstrap
                    --    窗口先发布给 guest。完成初始几何后再启用同一个正式 QEMU 能力，
                    --    既保持 3008×1692 首 mode，又让后续用户 resize 进入 virtio-gpu。
                    set zoomItem to menu item "Zoom To Fit" of menu 1 of menu bar item "View" of menu bar 1 of targetProcess
                    if value of attribute "AXMenuItemMarkChar" of zoomItem is missing value then click zoomItem
                    set frontmost of targetProcess to true
                    return
                end if
            end if
            delay 0.05
        end repeat
    end tell
    error "process " & processId & " did not register as a Cocoa application"
end run
