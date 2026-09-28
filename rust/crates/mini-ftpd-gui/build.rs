// Windows 目标下把 assets/icon.ico 嵌入 exe 资源（任务栏/资源管理器图标）
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/icon.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
