// Windows only treats us as a modern app (Windows 10+, comctl32 v6, per-monitor DPI v2) with a manifest.
// Without one, the shell hands back a generic icon for packaged apps (Outlook, Terminal, Settings…).
use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("MacDock").dpi_awareness(DpiAwareness::PerMonitorV2)).expect("unable to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
