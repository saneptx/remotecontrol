#!/bin/bash
# 远程控制期间压住 AWDL（AirDrop / 接力 / 通用控制使用的无线通道）。
#
# AWDL 会让 Wi-Fi 网卡周期性跳到其它信道，造成几十到上百毫秒的停顿，是 Wi-Fi 下
# 远程画面延迟尖峰的主要来源；macOS 会在附近有苹果设备时自动重新打开它。
# 本脚本安装一个后台守护：只要有远程画面窗口（rc-client view）在运行，就保持 awdl0 关闭；退出后
# 不再干预（系统会按需重新打开，AirDrop 等功能照常可用）。
#
# 用法：
#   sudo ./awdl-guard.sh install     安装并启动守护（开机自动运行）
#   sudo ./awdl-guard.sh uninstall   卸载并恢复 AWDL
#   ./awdl-guard.sh status           查看状态
set -euo pipefail

LABEL=com.remotecontrol.awdl-guard
PLIST=/Library/LaunchDaemons/$LABEL.plist
BIN=/usr/local/libexec/remotecontrol-awdl-guard

need_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "请用 sudo 运行：sudo $0 $1" >&2
        exit 1
    fi
}

case "${1:-}" in
install)
    need_root install
    mkdir -p "$(dirname "$BIN")"
    cat > "$BIN" <<'GUARD'
#!/bin/bash
# 每秒检查一次：有远程画面窗口在运行且 awdl0 处于开启状态时把它关掉。
while true; do
    if pgrep -qf "rc-client view" && ifconfig awdl0 2>/dev/null | head -1 | grep -q "<UP"; then
        ifconfig awdl0 down
    fi
    sleep 1
done
GUARD
    chmod 755 "$BIN"
    cat > "$PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key><array><string>$BIN</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
</dict>
</plist>
PL
    launchctl bootout system "$PLIST" 2>/dev/null || true
    launchctl bootstrap system "$PLIST"
    echo "已安装：rc-client 运行期间会自动关闭 AWDL。卸载：sudo $0 uninstall"
    ;;
uninstall)
    need_root uninstall
    launchctl bootout system "$PLIST" 2>/dev/null || true
    rm -f "$PLIST" "$BIN"
    ifconfig awdl0 up 2>/dev/null || true
    echo "已卸载，AWDL 已恢复"
    ;;
status)
    if launchctl print "system/$LABEL" >/dev/null 2>&1; then echo "守护：运行中"; else echo "守护：未安装"; fi
    ifconfig awdl0 | grep -E "status|flags" | sed 's/^/awdl0: /'
    ;;
*)
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
