# SuperControl

局域网远程控制工具：Windows 与 Mac 互相控制，目标是超低延迟与高保真画面。两台电脑都打开 SuperControl，
就能在“设备”列表里看到对方并一键连接。

| | 实现 |
|---|---|
| 采集 | DXGI Desktop Duplication（画面无变化时不发送任何数据） |
| 编码 | 默认 HEVC 4:2:0，控制端解不了 HEVC 时（Windows 没装“HEVC 视频扩展”）自动改用 H.264；连接时双方协商。被控端编码器（Windows：优先 NVENC，没有 NVIDIA 显卡时用系统的硬件编码器，即 Intel Quick Sync / AMD VCN 的 Media Foundation 编码器；Mac：VideoToolbox），超低延迟设置，无 B 帧，CBR + 单帧 VBV，显存内零拷贝；按控制端窗口的实际像素尺寸在显卡上缩小后再编码 |
| 传输 | 视频走 UDP（≤1400 字节分片 + Reed-Solomon 前向纠错，零星丢包直接还原，不重传）；控制走 TCP_NODELAY |
| 解码显示 | VideoToolbox 硬解 + AVSampleBufferDisplayLayer，解完立即上屏，不缓冲 |
| 输入 | 绝对坐标鼠标、滚轮、扫描码键盘，经 SendInput 注入；本地光标跟随被控端的光标形状（箭头、I 形、手指、调整大小等） |
| 发现 | UDP 广播，打开控制端即可看到局域网里所有被控端 |
| 声音 | WASAPI 环回采集被控端正在播放的声音，原始 PCM 经 UDP 传输（不压缩、无编解码延迟），控制端抖动缓冲约 30 ms 后播放 |
| 多显示器 | 被控端有多块显示器时，在主界面“正在控制”里点显示器名称切换，或在远程窗口里按 ⌃⌥← / ⌃⌥→（Windows 上 Ctrl+Alt+← / →） |
| 托盘与自启 | 关闭窗口时隐藏到菜单栏 / 任务栏托盘，被控端继续运行；可设置开机自动在后台启动 |
| 剪贴板与文件 | 独立的数据通道（不影响键鼠响应）：文字、图片双向自动同步；本机复制的文件在远程画面里直接粘贴（或拖进窗口）即可发送，远端复制的文件按 ⌘⇧D 取回 |
| 认证 | 首次连接配对（X25519 交换密钥 + 双方核对 6 位配对码 + 被控端弹窗确认），之后免密；每次连接双向挑战应答，防止冒充被控端 |

## 目录

- `crates/protocol` — 协议：控制消息、UDP 分片/重组、HEVC Annex-B 解析
- `crates/host` — 被控端（Windows；其它平台只有用于联调的测试图像源）
- `crates/client` — SuperControl 界面与控制端（macOS / Windows）
- `packaging/icon` — 应用图标（`draw-icon.swift` 绘制，`build-icons.sh` 生成 .icns / .ico）

## 下载

不想自己编译的话，GitHub Actions 每次推送都会自动构建：

- 最新构建：仓库的 **Actions** 页 → 最近一次成功的 `build` → 页面底部 Artifacts 里的 `SuperControl-windows-x64` /
  `SuperControl-macos-arm64`
- 正式版本：推送 `v*` 标签（如 `git tag v0.1.0 && git push origin v0.1.0`）后会自动发布到 **Releases**

Windows 包里只有两个文件，放在同一个文件夹，不需要安装运行库（被控需要支持 HEVC 硬件编码的显卡：NVIDIA GTX 10 系、Intel 6 代酷睿核显、AMD RX 400 系及以上）：

- `SuperControl.exe` — 界面，双击打开；
- `rc-host.exe` — 被控端，由界面调用，不需要手动运行。

第一次打开后在“设置 → 被控制”里点“安装”（会请求一次管理员权限）：程序装到 `C:\Program Files\SuperControl\`，
开始菜单出现 SuperControl。之后锁屏、登录界面、UAC 弹窗和以管理员身份运行的程序都能正常显示和操作。

被控端只在 SuperControl 打开期间运行：打开时自动启动，关闭后自动停止。两台电脑都打开 SuperControl 才能互相控制，
“设置”里可以用“允许其他设备控制这台电脑”临时关闭。

## 系统服务模式

```
rc-host.exe install [--name 书房电脑] [--port 47800] [--bitrate 40] [--fps 60] [--fec 20]
rc-host.exe uninstall
rc-host.exe devices            # 列出已配对的控制端
rc-host.exe forget <名称>       # 删除已配对的控制端（--all 删除全部）
```

需要在管理员终端里执行（界面里的“安装 / 卸载”按钮做的就是这个）。`install` 会：

1. 把 `rc-host.exe` 和同目录的 `SuperControl.exe` 复制到 `C:\Program Files\SuperControl\`，并在开始菜单创建快捷方式；
2. 创建 `C:\ProgramData\RemoteControl\`（仅 SYSTEM 和管理员可读，沿用旧名以保留配对），保存本机标识与已配对设备；
3. 添加防火墙入站规则（专用 / 域网络）；
4. 注册手动启动的系统服务（允许普通用户启动 / 停止，由界面程序启动），崩溃后自动重启；
   界面关闭（或崩溃）后服务在几秒内自行停止。

修改参数：带上新参数重新执行 `install`。升级版本：用新版 exe 重新执行 `install`（配对信息保留）。

运行日志在 `C:\ProgramData\RemoteControl\host.log`（需要管理员权限查看）。

**连不上时**：在管理员终端运行 `rc-host.exe status`，它会检查服务是否运行、端口是否在监听、
网络是否被识别为“公用网络”、防火墙规则是否存在，并显示最近的日志。

**原理**：服务运行在看不到屏幕的 session 0 里，它在当前登录会话中以 SYSTEM 身份启动工作进程；
SYSTEM 身份可以进入锁屏 / UAC 所在的安全桌面，工作进程在桌面切换时自动跟随，继续采集和注入输入。

**注意**：
- 用户注销或切换用户时，服务会在新会话里重启工作进程，控制端需要重新连接。
- 不支持发送 Ctrl+Alt+Del（Windows 不允许模拟）。如果锁屏要求先按 Ctrl+Alt+Del，请在本机组策略中关闭该要求。
- 如果 Windows 把当前网络识别为“公用网络”，防火墙规则不生效，请把网络改为“专用网络”。

## 使用方法

1. Windows 上安装被控端（见上文“系统服务模式”）。
2. Mac 上打开 `SuperControl.app`（`packaging/macos/make-app.sh --install` 生成并安装到“应用程序”）。
   首次打开时 macOS 会询问是否允许访问本地网络，请选择“允许”。
3. 界面会自动列出局域网里的被控端，点“连接”。
4. **首次连接需要配对**：Mac 上显示一个 6 位配对码，Windows 上同时弹出确认框显示同一个配对码，
   核对一致后在 Windows 上点“是”。之后这台 Mac 可以直接连接，无需任何确认或密码。
   不一致说明可能有人在中间篡改，请点“否”。

### 剪贴板与文件传输

- **剪贴板**：文字和图片在两台电脑之间自动双向同步（Mac 的 ⌘C 对应 Windows 的 Ctrl+C）。
- **本机 → 远端传文件**：在本机复制文件（访达 ⌘C / 资源管理器 Ctrl+C），到远程画面里打开目标文件夹按粘贴键，
  文件会先传过去（保存到远端“下载”并放进远端剪贴板），传完自动在远端粘贴。也可以直接把文件拖进远程画面窗口。
- **Windows → Mac 传文件**：在 Windows 资源管理器里复制文件（Ctrl+C），窗口标题会提示“远端复制了 …”，
  按 **⌘⇧D** 保存到 Mac 的“下载”文件夹，同时放进 Mac 剪贴板，可在访达里 ⌘V 粘贴。
- 首次接收文件时 macOS 可能询问是否允许 SuperControl 访问“下载”文件夹，请选择允许。

界面里的设置：

- **播放远端的声音**：默认开启
- **Command 键当作 Ctrl**：⌘C / ⌘V 直接对应 Windows 的复制粘贴（默认开启）
- **按原始分辨率传输**：默认按窗口的实际像素尺寸缩小编码（延迟更低、显示效果相同）
- **远程期间自动关闭 AWDL**：减少 Wi-Fi 延迟尖峰，首次开启需要输入管理员密码（见下文）

### 命令行

```
rc-client                       # 打开图形界面
rc-client scan                  # 扫描局域网里的被控端
rc-client view 192.168.1.20     # 直接打开远程画面（未配对时在终端显示配对码）
    [--cmd-as-ctrl] [--native] [--stats] [--send 文件或文件夹 ...]
rc-client hosts / forget <名称>  # 查看 / 删除已配对的被控端
```

### 从源码构建

被控端（Windows，需要支持 HEVC 硬件编码的显卡与较新驱动、Rust MSVC 工具链；可用 `--encoder nvenc|mf` 指定编码器）：

```
cargo build --release -p rc-host
```

控制端（macOS）：

```
packaging/macos/create-signing-cert.sh # 第一次：在登录钥匙串里创建本地签名证书（只需一次）
packaging/macos/make-app.sh            # 生成 target/SuperControl.app（加 --install 装到“应用程序”）
```

有本地签名证书时，更新后“辅助功能”“录屏与系统录音”的授权依然有效；没有证书只能临时签名，
每次重新编译都要在系统设置里删掉旧的 SuperControl 再重新授权。

被控端常用参数：

- `--fps`：帧率上限，默认跟随显示器刷新率（120/144Hz 显示器即可推 120/144 帧）
- `--fec`：前向纠错冗余比例，默认 20（%）；有线网络可降到 5～10
- `--allow-sleep`：允许系统睡眠（默认运行期间阻止睡眠，否则睡眠后无法连接）

### 统计信息怎么看

窗口标题（或 `--stats`）每秒显示一次：

- **fps**：每秒实际收到的画面数。画面静止时被控端不发送，所以这个数会下降，这不是卡顿。
- **延迟**：从远端画面更新到本机提交解码的耗时，拆成“被控端”（等待采集 + 编码）和“传输”两部分；不含本机解码上屏与显示器刷新（通常再加 5～15 ms）。
- **FEC 修复**：有丢包、但靠冗余数据还原的帧数。持续很高说明网络丢包严重。
- **丢帧**：冗余也救不回来的帧，会短暂卡住直到下一个关键帧到达。

### 网络建议（对延迟影响最大）

- **两端尽量都走有线**。Wi-Fi 是延迟抖动和丢包的主要来源；Mac 可用 USB-C 转网口。
- **2.4 GHz Wi-Fi 带宽不够**，Mac 和 Windows 都要连 5 GHz（或 6 GHz）网络。
- Mac 用 Wi-Fi 时，AirDrop / 接力 / 通用控制使用的 AWDL 会周期性切换信道，造成几十到上百毫秒的停顿
  （窗口标题会提示“AWDL 开启”）。macOS 会自动把它重新打开，推荐在界面里勾选“远程期间自动关闭 AWDL”：
  它会安装一个后台守护，只在远程画面打开期间压住 AWDL，其余时间隔空投送等功能照常可用。
  （等价的命令行：`sudo packaging/macos/awdl-guard.sh install` / `uninstall`）
- 不开串流时先 `ping` 一下被控端：如果 RTT 本身就忽高忽低，问题在网络，不在本程序。

没有 Windows 机器时，可在 macOS 上直接运行 `rc-host`（需要带 libx265 的 ffmpeg），
它会推送一路 HEVC 测试图像，用来联调控制端。

## 已知限制

- 画面里没有远端鼠标指针（Desktop Duplication 不含指针），目前靠本地指针定位，指针形状不会跟随变化。
- 画面统一为 HEVC 4:2:0（8 bit）：彩色细节（例如彩色小字的边缘）比 4:4:4 略软，换来约一半的码率，且两个方向的硬件编解码器都支持。
- 只采集主显示器；混合显卡笔记本（显示器接在核显上）暂不支持。
- 直接双击运行时看不到锁屏界面与 UAC 弹窗（窗口标题会提示），请安装为系统服务。
- macOS 的系统级快捷键（Cmd+Tab、Cmd+Space、Mission Control 等）会被本机截获，发不到远端。
- 视频与键鼠数据未加密（配对密钥不在网络上传输），仅适用于可信局域网。
