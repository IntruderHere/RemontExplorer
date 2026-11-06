//! Сборка GUI: для Windows-таргетов встраивает иконку (icons/app.rc).
//! На других таргетах (разработка на macOS) ресурсы не собираются.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        println!("cargo:rerun-if-changed=icons/app.rc");
        println!("cargo:rerun-if-changed=icons/filemanager.ico");
        embed_resource::compile("icons/app.rc", embed_resource::NONE);
    }
}
