fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !windows_msvc {
        tauri_build::build();
        return Ok(());
    }
    // tauri-build embeds its app manifest (Common Controls v6) only in the
    // app binary, and a `tauri::test` binary without it fails to start with
    // STATUS_ENTRYPOINT_NOT_FOUND. The same manifest, embedded by the linker,
    // reaches every target: the app, its tests and its examples.
    embed_manifest()?;
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest()),
    )?;
    Ok(())
}

#[allow(clippy::print_stdout, reason = "cargo reads build-script directives from stdout")]
fn embed_manifest() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?).join("windows/app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    // Executables only (not the cdylib), and without the UAC section the
    // linker adds by default, so the result is exactly tauri-build's manifest.
    for target in ["bins", "tests", "examples"] {
        println!("cargo:rustc-link-arg-{target}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-{target}=/MANIFESTUAC:NO");
        println!("cargo:rustc-link-arg-{target}=/MANIFESTINPUT:{}", manifest.display());
    }
    Ok(())
}
