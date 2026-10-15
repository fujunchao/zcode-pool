fn main() {
    println!("cargo:rerun-if-changed=icons");
    let attrs = tauri_build::Attributes::new().plugin(
        "reg-driver",
        tauri_build::InlinedPlugin::new().commands(&["report"]),
    );
    tauri_build::try_build(attrs).expect("生成注册驱动最小权限失败")
}
