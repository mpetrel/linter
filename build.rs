use std::path::Path;

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-light".into())
        .with_edition((1, 19));
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

/// Embeds the icon, version info and manifest in mdedit.exe.
fn windows_resources() {
    let template = "packaging/windows/mdedit.rc.in";
    for f in [template, "packaging/windows/mdedit.manifest", "assets/mdedit.ico"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // rc.exe and windres both accept forward slashes, which need no escaping.
    let path = |rel: &str| Path::new(&root).join(rel).to_string_lossy().replace('\\', "/");
    let env = |name: &str| std::env::var(name).unwrap();
    let rc = std::fs::read_to_string(template)
        .unwrap()
        .replace("@ICON@", &path("assets/mdedit.ico"))
        .replace("@MANIFEST@", &path("packaging/windows/mdedit.manifest"))
        .replace("@MAJOR@", &env("CARGO_PKG_VERSION_MAJOR"))
        .replace("@MINOR@", &env("CARGO_PKG_VERSION_MINOR"))
        .replace("@PATCH@", &env("CARGO_PKG_VERSION_PATCH"))
        .replace("@VERSION@", &env("CARGO_PKG_VERSION"));
    let out = Path::new(&env("OUT_DIR")).join("mdedit.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_required().expect("Windows resources");
}
