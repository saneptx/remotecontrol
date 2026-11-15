# remotecontrol

局域网远程控制工具：**Windows 被控端 → macOS 控制端**，目标是超低延迟与高保真画面。

| | 实现 |
|---|---|
| 采集 | DXGI Desktop Duplication（画面无变化时不发送任何数据） |
| 编码 | NVENC HEVC **4:4:4**，P1 预设 + 超低延迟调优，无 B 帧，CBR + 单帧 VBV，显存内零拷贝 |
| 传输 | 视频走 UDP（≤1400 字节分片 + Reed-Solomon 前向纠错，零星丢包直接还原，不重传）；控制走 TCP_NODELAY |
| 解码显示 | VideoToolbox 硬解 + AVSampleBufferDisplayLayer，解完立即上屏，不缓冲 |
| 输入 | 绝对坐标鼠标、滚轮、扫描码键盘，经 SendInput 注入 |
| 认证 | 密码 + HMAC-SHA256 挑战应答（密码不在网络上明文传输） |

## 目录

- `crates/protocol` — 协议：控制消息、UDP 分片/重组、HEVC Annex-B 解析
- `crates/host` — 被控端（Windows；其它平台只有用于联调的测试图像源）
- `crates/client` — 控制端（macOS）

## 下载

不想自己编译的话，GitHub Actions 每次推送都会自动构建：

- 最新构建：仓库的 **Actions** 页 → 最近一次成功的 `build` → 页面底部 Artifacts 里的 `rc-host-windows-x64`
- 正式版本：推送 `v*` 标签（如 `git tag v0.1.0 && git push origin v0.1.0`）后会自动发布到 **Releases**

`rc-host.exe` 是单个文件，不需要安装任何运行库（需要 NVIDIA 显卡与较新驱动）。两种用法：

- **临时使用**：双击 `rc-host.exe`，窗口里显示连接密码。首次运行时防火墙会弹窗，选择允许“专用网络”。
  这种方式看不到也操作不了锁屏和 UAC 弹窗。
- **安装为系统服务（推荐）**：双击 `install-service.cmd`（会请求管理员权限）。之后开机自动运行，
  锁屏、登录界面、UAC 弹窗和以管理员身份运行的程序都能正常显示和操作。详见下文。

## 系统服务模式

```
rc-host.exe install [--password 密码] [--port 47800] [--bitrate 80] [--fps 60] [--fec 20]
rc-host.exe uninstall
```

需要在管理员终端里执行（或直接双击 `install-service.cmd` / `uninstall-service.cmd`）。`install` 会：

1. 把程序复制到 `C:\Program Files\RemoteControl\`；
2. 保存连接密码到 `C:\ProgramData\RemoteControl\password.txt`（仅 SYSTEM 和管理员可读）。
   不指定 `--password` 时首次安装随机生成，之后重新安装会沿用原密码；
3. 添加防火墙入站规则（专用 / 域网络）；
4. 注册并启动开机自启的系统服务，崩溃后自动重启。

修改密码或参数：带上新参数重新执行 `install`。升级版本：用新版 exe 重新执行 `install`。

运行日志在 `C:\ProgramData\RemoteControl\host.log`（需要管理员权限查看）。

**原理**：服务运行在看不到屏幕的 session 0 里，它在当前登录会话中以 SYSTEM 身份启动工作进程；
SYSTEM 身份可以进入锁屏 / UAC 所在的安全桌面，工作进程在桌面切换时自动跟随，继续采集和注入输入。

**注意**：
- 用户注销或切换用户时，服务会在新会话里重启工作进程，控制端需要重新连接。
- 不支持发送 Ctrl+Alt+Del（Windows 不允许模拟）。如果锁屏要求先按 Ctrl+Alt+Del，请在本机组策略中关闭该要求。
- 如果 Windows 把当前网络识别为“公用网络”，防火墙规则不生效，请把网络改为“专用网络”。

## 构建与运行

被控端（Windows，需要 NVIDIA GTX 10 系及以上显卡、较新驱动、Rust MSVC 工具链）：

```
cargo build --release -p rc-host
target\release\rc-host.exe --bitrate 80 --fps 60
```

启动后会打印连接密码（也可用 `--password` 或环境变量 `RC_PASSWORD` 固定）。

控制端（macOS）：

```
cargo build --release -p rc-client
target/release/rc-client 192.168.1.20 -p <密码>
```

- `--cmd-as-ctrl`：把 Command 当作 Ctrl 发送（默认当作 Win 键）
- `--stats`：每秒在终端打印帧率 / 码率 / 延迟

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
- Mac 用 Wi-Fi 时，AirDrop / 接力使用的 AWDL 会周期性切换信道，造成几十毫秒的停顿。
  测试时可临时关闭：`sudo ifconfig awdl0 down`（重启或 `sudo ifconfig awdl0 up` 恢复，期间 AirDrop 不可用）。
- 不开串流时先 `ping` 一下被控端：如果 RTT 本身就忽高忽低，问题在网络，不在本程序。

没有 Windows 机器时，可在 macOS 上直接运行 `rc-host`（需要带 libx265 的 ffmpeg），
它会推送一路 HEVC 4:4:4 测试图像，用来联调控制端。

## 已知限制

- 画面里没有远端鼠标指针（Desktop Duplication 不含指针），目前靠本地指针定位，指针形状不会跟随变化。
- NVENC 内部的 RGB→YUV 转换是 8 bit BT.601 limited range，属于“视觉近无损”而非逐像素一致。
- 只采集主显示器；混合显卡笔记本（显示器接在核显上）暂不支持。
- 直接双击运行时看不到锁屏界面与 UAC 弹窗（窗口标题会提示），请安装为系统服务。
- macOS 的系统级快捷键（Cmd+Tab、Cmd+Space、Mission Control 等）会被本机截获，发不到远端。
- 视频流未加密，仅适用于可信局域网。
- 无音频、剪贴板、文件传输。
