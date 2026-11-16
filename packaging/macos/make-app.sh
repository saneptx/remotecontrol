#!/bin/bash
# 构建并打包成 SuperControl.app（输出到 target/SuperControl.app）。
# 用法：packaging/macos/make-app.sh [--install]   --install 时复制到 /Applications
set -euo pipefail
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
</dict>
</plist>
PLIST

# 本地临时签名：系统权限（本地网络等）才能稳定地记在这个应用上
codesign --force --sign - --identifier com.remotecontrol.host "$APP/Contents/MacOS/rc-host" >/dev/null
codesign --force --sign - "$APP" >/dev/null
echo "已生成 $APP"

if [ "${1:-}" = "--install" ]; then
    rm -rf /Applications/RemoteControl.app /Applications/SuperControl.app  # RemoteControl.app 是改名前的旧版本
    cp -R "$APP" /Applications/
    echo "已安装到 /Applications/SuperControl.app"
fi
