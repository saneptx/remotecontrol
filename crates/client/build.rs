// Windows：把应用图标和版本信息嵌入 exe（资源管理器、任务栏显示的图标）。
fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icon/SuperControl.ico");
    println!("cargo:rerun-if-changed=app.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
