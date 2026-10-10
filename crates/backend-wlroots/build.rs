//! Mirrors freerdp-sys's `freerdp_present` cfg into backend-wlroots (F1:
//! only with the WinPR tables present can we translate RDP scancodes to
//! evdev keycodes properly).

fn main() {
    println!("cargo::rustc-check-cfg=cfg(freerdp_present)");
    if std::env::var("DEP_FREERDP3_PRESENT").is_ok() {
        println!("cargo:rustc-cfg=freerdp_present");
    }
}
