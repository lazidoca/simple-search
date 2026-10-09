fn main() {
    // Windows exe icon (taskbar + pinned shortcut). Never fail the build if
    // the resource compiler is missing — the runtime window icon still applies.
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("warning: exe icon skipped ({e})");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
