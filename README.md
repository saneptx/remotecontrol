# remotecontrol

局域网远程控制工具：**Windows 被控端 → macOS 控制端**，目标是超低延迟与高保真画面。

| | 实现 |
|---|---|
| 采集 | DXGI Desktop Duplication（画面无变化时不发送任何数据） |
| 编码 | NVENC HEVC **4:4:4**，P1 预设 + 超低延迟调优，无 B 帧，CBR + 单帧 VBV，显存内零拷贝 |
| 传输 | 视频走 UDP（≤1400 字节分片，丢包不重传、直接请求关键帧）；控制走 TCP_NODELAY |
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
- 窗口标题实时显示统计；“延迟”指 采集 → 提交解码 的耗时（不含解码上屏与显示器刷新）

没有 Windows 机器时，可在 macOS 上直接运行 `rc-host`（需要带 libx265 的 ffmpeg），
它会推送一路 HEVC 4:4:4 测试图像，用来联调控制端。

## 已知限制

- 画面里没有远端鼠标指针（Desktop Duplication 不含指针），目前靠本地指针定位，指针形状不会跟随变化。
- NVENC 内部的 RGB→YUV 转换是 8 bit BT.601 limited range，属于“视觉近无损”而非逐像素一致。
- 只采集主显示器；混合显卡笔记本（显示器接在核显上）暂不支持；UAC 安全桌面和锁屏界面采集不到（需以系统服务运行）。
- macOS 的系统级快捷键（Cmd+Tab、Cmd+Space、Mission Control 等）会被本机截获，发不到远端。
- 视频流未加密，仅适用于可信局域网。
- 无音频、剪贴板、文件传输。
