//! Embed airtalk.ico + VersionInfo into the Windows PE resources of
//! airtalk.exe. Without this, Alt+Tab and the taskbar show the Windows
//! default executable icon. No-op on non-Windows targets.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/airtalk.ico");
        res.set("ProductName", "AirTalk");
        res.set("FileDescription", "AirTalk voice input");
        res.set("CompanyName", "jeffcaiz");
        res.set("LegalCopyright", "Copyright (c) 2026 Jeff Cai");
        if let Err(e) = res.compile() {
            // Don't break the build if rc.exe isn't present — keep the
            // iconless binary working for devs on minimal toolchains.
            // CI always has the Windows SDK so release builds embed it.
            println!("cargo:warning=winres compile failed: {e}");
        }
    }
}
