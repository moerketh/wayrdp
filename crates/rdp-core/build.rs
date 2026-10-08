//! Mirrors freerdp-sys's `freerdp_present` cfg into rdp-core.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(freerdp_present)");
    if std::env::var("DEP_FREERDP3_PRESENT").is_ok() {
        println!("cargo:rustc-cfg=freerdp_present");
    }
}
