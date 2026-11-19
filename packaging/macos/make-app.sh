#!/bin/bash
# 构建并打包成 SuperControl.app（输出到 target/SuperControl.app）。
# 用法：packaging/macos/make-app.sh [--install]   --install 时复制到 /Applications
set -euo pipefail
# 出错时报出行号和命令；在 GitHub Actions 里以 ::error:: 注释显示（不登录也能看到）
trap 'echo "::error::make-app.sh 第 $LINENO 行失败（退出码 $?）：$BASH_COMMAND"' ERR
cd "$(dirname "$0")/../.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

cargo build --release -p rc-client -p rc-host
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=target/SuperControl.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/rc-client "$APP/Contents/MacOS/rc-client"
cp target/release/rc-host "$APP/Contents/MacOS/rc-host"
cp packaging/icon/SuperControl.icns "$APP/Contents/Resources/SuperControl.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>SuperControl</string>
    <key>CFBundleDisplayName</key><string>SuperControl</string>
    <key>CFBundleIconFile</key><string>SuperControl</string>
    <key>CFBundleIdentifier</key><string>com.remotecontrol.client</string>
    <key>CFBundleExecutable</key><string>rc-client</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>用于发现并连接同一局域网中的电脑。</string>
    <key>NSMicrophoneUsageDescription</key><string>用于把本机麦克风的声音传给正在控制的电脑（在设置里开启后才会使用）。</string>
</dict>
</plist>
PLIST

# 签名：有本地证书（packaging/macos/create-signing-cert.sh 创建）就用它，否则临时签名。
# 临时签名每次编译都会变，macOS 会把之前给的“辅助功能”“录屏”授权当作别的程序；固定证书签名则一直有效。
IDENTITY="SuperControl Local Signing"
if ! security find-identity -p codesigning | grep -q "\"$IDENTITY\""; then
    echo "提示：没有找到证书「$IDENTITY」，使用临时签名（每次更新后需要重新授权）。运行 packaging/macos/create-signing-cert.sh 创建" >&2
    IDENTITY=-
fi
codesign --force --sign "$IDENTITY" --identifier com.remotecontrol.host "$APP/Contents/MacOS/rc-host" >/dev/null
codesign --force --sign "$IDENTITY" "$APP" >/dev/null
echo "已生成 $APP"

if [ "${1:-}" = "--install" ]; then
    rm -rf /Applications/RemoteControl.app /Applications/SuperControl.app  # RemoteControl.app 是改名前的旧版本
    cp -R "$APP" /Applications/
    echo "已安装到 /Applications/SuperControl.app"
fi
