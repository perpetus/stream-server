// use chrono::{Datelike, Local};
// use std::env;

fn main() {
    #[cfg(target_os = "windows")]
    {
        let now = Local::now();
        let copyright = format!("Copyright © {} Perpetus", now.year());
        let exe_name = format!("{}.exe", env::var("CARGO_PKG_NAME").unwrap());

        let mut res = winres::WindowsResource::new();
        res.set_manifest(
            r#"
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
        <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
        <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
</application>
</assembly>
"#,
        );
        res.set(
            "FileDescription",
            "Stream Server - Torrent Streaming Server",
        );
        res.set("LegalCopyright", &copyright);
        res.set("OriginalFilename", &exe_name);
        res.set_icon_with_id("../icons/app.ico", "MAINICON");
        res.compile().unwrap();
    }
}
