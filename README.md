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

`rc-host.exe` 是单个文件，不需要安装任何运行库，双击即可运行（需要 NVIDIA 显卡与较新驱动）。
首次运行时 Windows 防火墙会弹窗，选择允许“专用网络”。

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
- **锁屏界面与 UAC 弹窗看不到也操作不了**（窗口标题会提示）：普通权限的程序无权访问安全桌面，需要以系统服务方式运行才能解决。
  在此之前，如果希望熄屏后能直接唤醒使用，请在 Windows“设置 → 账户 → 登录选项”里把“你离开后，Windows 应在何时要求你重新登录”设为“从不”。
- macOS 的系统级快捷键（Cmd+Tab、Cmd+Space、Mission Control 等）会被本机截获，发不到远端。
- 视频流未加密，仅适用于可信局域网。
- 无音频、剪贴板、文件传输。
