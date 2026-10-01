fn main() {
    // Embed a Windows manifest. The app runs `asInvoker` (unelevated): indexing
    // is delegated to the optional LocalSystem service, and the few operations
    // that need admin relaunch elevated on demand.
    let windows =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("AllTheThings.manifest"));
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("failed to run tauri-build");
}
