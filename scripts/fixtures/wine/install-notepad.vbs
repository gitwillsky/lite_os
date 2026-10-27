' 标准 Win32 安装样本：复制明确的 i386 PE，并由 IShellLink 生成桌面/开始菜单快捷方式。
Option Explicit
Dim shell, files, folder, executable, shortcut
Set shell = CreateObject("WScript.Shell")
Set files = CreateObject("Scripting.FileSystemObject")
folder = files.BuildPath(shell.ExpandEnvironmentStrings("%ProgramFiles%"), "LiteOS Win32 Notepad")
If Not files.FolderExists(folder) Then files.CreateFolder(folder)
executable = files.BuildPath(folder, "notepad.exe")
files.CopyFile "Z:\opt\wine\lib\wine\i386-windows\notepad.exe", executable, True
Set shortcut = shell.CreateShortcut(files.BuildPath(shell.SpecialFolders("Desktop"), "Win32 Notepad.lnk"))
shortcut.TargetPath = executable
shortcut.WorkingDirectory = folder
shortcut.Arguments = "Z:\var\tmp\win32-note.txt"
shortcut.Save
Set shortcut = shell.CreateShortcut(files.BuildPath(shell.SpecialFolders("AllUsersPrograms"), "Win32 Notepad.lnk"))
shortcut.TargetPath = executable
shortcut.WorkingDirectory = folder
shortcut.Arguments = "Z:\var\tmp\win32-note.txt"
shortcut.Save
WScript.Echo "WIN32_" & "INSTALL_OK"
