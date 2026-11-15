//! macOS：ScreenCaptureKit 绑定库里有 Swift 代码，链接时需要 Swift 运行时的兼容库。
//! 它的构建脚本只按 Xcode 的目录结构添加搜索路径，只装了命令行工具时找不到，这里按实际的 swift 位置补上。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(out) = std::process::Command::new("xcrun").args(["--find", "swift"]).output() else { return };
    let swift = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if let Some(toolchain) = swift.parent().and_then(|p| p.parent()) {
        let dir = toolchain.join("lib/swift/macosx");
        if dir.exists() {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }
    // 运行时从系统自带的 Swift 运行库加载（/usr/lib/swift 在 dyld 共享缓存里）
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rerun-if-changed=build.rs");
}
