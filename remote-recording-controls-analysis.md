# Windows 远程环境下录制控制栏不可见问题分析

## 问题现象

在 Windows 主机上通过网易 UU 远程（进程和驱动名称为 GameViewer）操作 Cap 时，录制可以正常开始和结束，但录制期间看不到包含停止、暂停等操作的控制栏。

本次测试使用的程序为：

```text
E:\work\Cap\target\release\Cap - Development.exe
```

桌面程序的普通日志和错误日志位于：

```text
C:\Users\wenyd\AppData\Local\so.cap.desktop\logs
```

开发版的录制项目位于：

```text
C:\Users\wenyd\AppData\Roaming\so.cap.desktop.dev\recordings
```

## 日志分析

最新日志表明录制管线本身工作正常：

- DXGI 正确选择 NVIDIA GeForce RTX 4060。
- 屏幕捕获和 NVENC H.264 编码正常启动。
- 停止录制后，视频分片完成拼接和校验。
- 最终日志包含 `Successfully finalized fragmented recording`。

日志中还出现了以下告警，但它们不是控制栏不可见的原因：

- `Failed to get HWND for content protection diagnostics`：停止录制时，窗口/区域捕获使用的 Occluder 窗口已经销毁，诊断代码无法再读取它的 HWND。
- WebSocket `10053`：编辑器预览页面关闭或切换时，本机中止了 WebSocket 连接。
- `File fsync failed ... 拒绝访问`：最后一个 M4S 分片刷盘失败，但程序随后通过未列出分片恢复路径完成了分片处理和最终视频生成。
- `Previous Cap session terminated without a clean shutdown`：之前的一次 Cap 会话未正常退出，与本次控制栏显示问题无直接关系。

## 源码路径分析

录制开始时，`recording.rs` 会创建 `ShowCapWindow::InProgressRecording`。对应的窗口在 `windows.rs` 中配置为透明、置顶并显示在所有工作区：

```text
title: Cap Recording Controls
size: 320 × 150
always_on_top: true
transparent: true
visible_on_all_workspaces: true
```

默认排除窗口列表包含 `Cap Recording Controls`。录制激活后，Cap 会对这些窗口启用 Windows 内容保护，最终对应：

```text
WDA_EXCLUDEFROMCAPTURE = 0x11
```

这个设置的正常目的，是让用户在本机仍能看到控制栏，但控制栏不会进入最终录制视频。

远控软件同样需要通过 Windows 捕获接口取得桌面画面。当远控软件无法取得带有 `WDA_EXCLUDEFROMCAPTURE` 标记的窗口时，控制栏不仅不会进入录制，也不会出现在远程端看到的桌面中。

## 远程环境自动检测

`apps/desktop/src-tauri/src/platform/win.rs` 会尝试识别捕获式远程桌面环境。当前检测包括：

- Windows `SM_REMOTESESSION`，用于识别 RDP 会话。
- CPUID 和 SMBIOS，用于识别虚拟机、云电脑和 Shadow 等环境。
- 已附加到桌面的虚拟显示适配器，标记包括 `parsec`、`spacedesk`、`iddsample`、`virtual display`、`usbmmidd`、`amyuni` 和 `shadow`。
- 环境变量 `CAP_WINDOW_CAPTURE_EXCLUSION`，用于手动覆盖自动检测。

只有带 `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` 状态的虚拟显示器才参与检测。

本机枚举结果如下：

```text
NVIDIA GeForce RTX 4060             Attached=True
GameViewer Virtual Display Adapter Attached=False
Parsec Virtual Display Adapter     Attached=False
```

UU 当前直接串流 NVIDIA 物理显示器。虽然 GameViewer 虚拟显示驱动已经安装，但它没有附加到桌面，因此现有检测逻辑会跳过该适配器。同时，现有代码也没有通过 GameViewer 活跃会话或进程判断 UU 远控状态。

因此自动检测返回“非远程捕获环境”，Cap 继续为控制栏设置 `WDA_EXCLUDEFROMCAPTURE`。

## 现场验证过程

第一次尝试使用普通 PowerShell 环境变量启动：

```powershell
$env:CAP_WINDOW_CAPTURE_EXCLUSION = "off"
& "E:\work\Cap\target\release\Cap - Development.exe"
```

录制期间的只读检查结果为：

```text
Window: Cap Recording Controls
Visible: True
Rect: (1150, 867) - (1470, 1017)
Display Affinity: 0x11
```

这证明控制栏窗口已经创建、处于可见状态且位于屏幕范围内，但仍然带有 `WDA_EXCLUDEFROMCAPTURE`。进一步读取运行进程的环境块发现：

```text
CAP_WINDOW_CAPTURE_EXCLUSION=<not present>
```

因此这次测试中环境变量没有传递到最终运行的 Cap 进程，不能用于判断关闭排除后是否仍有透明窗口合成问题。

第二次使用 `Start-Process -Environment` 显式传递变量：

```powershell
Start-Process `
	-FilePath "E:\work\Cap\target\release\Cap - Development.exe" `
	-Environment @{ CAP_WINDOW_CAPTURE_EXCLUSION = "off" }
```

随后日志明确记录：

```text
Skipping window capture exclusion: this desktop is viewed through a capture-based stream,
so excluded windows would be invisible to the user. Cap's windows will appear in recordings.
reason=CAP_WINDOW_CAPTURE_EXCLUSION env override
```

录制期间再次枚举窗口：

```text
Window: Cap Recording Controls
Visible: True
Rect: (1043, 994) - (1363, 1144)
Display Affinity: 0x0
```

UU 远程画面此时可以看到停止、暂停控制栏。这同时排除了控制栏未创建、窗口位于屏幕外以及 UU 无法合成 Tauri 透明窗口等可能性。

## 根因结论

已验证的直接根因是：

1. Cap 为了避免将自身控制栏录入视频，给控制栏设置了 `WDA_EXCLUDEFROMCAPTURE`。
2. UU 远程通过捕获方式传输物理显示器画面，因此也无法取得这个受保护窗口。
3. UU 使用的 GameViewer 虚拟显示适配器当前没有附加到桌面，现有自动检测无法识别这次活跃的 UU 远程连接。
4. 关闭窗口捕获排除后，控制栏立即可以在 UU 远程画面中显示。

## 对其他远控软件的影响

当前行为取决于连接方式，而不只取决于软件名称：

| 远控方式 | 当前识别情况 | 控制栏风险 |
| --- | --- | --- |
| Windows RDP | 通过 `SM_REMOTESESSION` 识别 | 通常可见 |
| spacedesk 已附加虚拟屏 | 通过虚拟显示器名称识别 | 通常可见 |
| Parsec 已附加虚拟屏 | 通过虚拟显示器名称识别 | 通常可见 |
| Parsec 直接串流物理屏 | 不一定能识别 | 可能不可见 |
| UU 直接串流物理屏 | 当前不能识别 | 不可见 |
| Sunshine/Moonlight 直接串流物理屏 | 当前没有专项检测 | 可能不可见 |

已安装但未附加的 Parsec 或 spacedesk 虚拟显示器不会触发现有检测。

## 临时解决方案

通过以下方式启动开发版，可以强制关闭 Cap 窗口的录屏排除：

```powershell
Start-Process `
	-FilePath "E:\work\Cap\target\release\Cap - Development.exe" `
	-Environment @{ CAP_WINDOW_CAPTURE_EXCLUSION = "off" }
```

该变量只作用于本次启动的进程。关闭窗口捕获排除后，UU 可以显示控制栏，但 Cap 控制栏、设置窗口、摄像头窗口等自身窗口可能进入最终录制视频。

## 永久修复方向

永久修复需要在保持本地用户默认排除 Cap 窗口的同时，可靠识别活跃的捕获式远控会话。

不能只把 `gameviewer` 加入现有虚拟显示器名称列表，因为本次 UU 连接期间 GameViewer 适配器为 `Attached=False`，仍会被当前逻辑跳过。

后续实现应重点评估：

- 是否存在能够准确表示 GameViewer 当前远程连接状态的进程、窗口、服务状态或本地 IPC 信号。
- Sunshine/Moonlight、Parsec 物理屏串流等是否需要相同检测机制。
- 避免仅凭常驻服务或已安装驱动判断，否则远控软件未连接时也会关闭捕获排除，导致 Cap 窗口被意外录入。
- 保留 `CAP_WINDOW_CAPTURE_EXCLUSION` 作为诊断和手动覆盖手段。

验收标准应包括：远程连接活跃时控制栏在远程端可见；没有远程连接时控制栏仍不进入录制；检测失败时日志能够明确记录判断依据和最终 Display Affinity。
