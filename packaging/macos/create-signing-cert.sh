#!/bin/bash
# 在“登录”钥匙串里创建一个本地自签名的代码签名证书「SuperControl Local Signing」，只用于打包 SuperControl。
#
# 为什么需要：没有证书时只能临时签名（ad-hoc），每次重新编译签名都会变，macOS 会把之前给的
# “辅助功能”“录屏与系统录音”授权当作别的程序而失效。用固定证书签名后，授权在更新后依然有效。
# 证书只在本机使用，不会被系统信任为开发者证书，也不能用来发布给别人。
#
# 用法：packaging/macos/create-signing-cert.sh        已存在时不重复创建
set -euo pipefail
NAME="SuperControl Local Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
    echo "证书「$NAME」已存在"
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
CNF
# 用系统自带的 LibreSSL：它生成的 .p12 能被 security 直接导入
/usr/bin/openssl req -new -x509 -newkey rsa:2048 -nodes -days 3650 -config "$TMP/cert.cnf" \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null
PASS=$(/usr/bin/openssl rand -hex 16)
/usr/bin/openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$NAME" \
    -out "$TMP/cert.p12" -passout "pass:$PASS"
# -T：允许 codesign 使用私钥而不弹出钥匙串访问确认
security import "$TMP/cert.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign >/dev/null
echo "已创建证书「$NAME」（登录钥匙串，有效期 10 年）"
