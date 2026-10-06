//! Build-time FreeRDP 3 discovery and binding generation.
//!
//! FreeRDP 3 dev headers only exist on Linux (Debian/Kali: `freerdp3-dev`).
//! When they are missing we compile the crate as an explicit stub so the rest
//! of the workspace (trait, server shell) still type-checks anywhere.

use std::env;
use std::path::PathBuf;

const LIBS: [&str; 3] = ["freerdp-server3", "freerdp3", "winpr3"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-env-changed=FREERDP3_PREFIX");
    println!("cargo::rustc-check-cfg=cfg(freerdp_present)");

    let mut ok = true;
    for lib in LIBS {
        if pkg_config::Config::new().probe(lib).is_err() {
            println!(
                "cargo:warning={lib} not found via pkg-config; \
                 freerdp-sys building as stub (install freerdp3-dev)"
            );
            ok = false;
            break;
        }
    }
    if !ok {
        return;
    }

    println!("cargo:rustc-cfg=freerdp_present");
    // Forwarded to dependents via DEP_FREERDP3_PRESENT (crate declares
    // `links = "freerdp3"`) so rdp-core compiles its real server path.
    println!("cargo:present=1");

    // Compile the C shim against the discovered headers. pkg-config only
    // emitted link info above; re-probe per-lib to collect include dirs
    // (headers live at /usr/include/freerdp3 and /usr/include/winpr3, which
    // neither cc nor bindgen's clang search by default) and link paths.
    let mut build = cc::Build::new();
    build.file("shim.c");
    let mut bindgen = bindgen::Builder::default().header("shim.c");
    for lib in LIBS {
        let l = pkg_config::Config::new().probe(lib).unwrap();
        for inc in &l.include_paths {
            build.include(inc);
            bindgen = bindgen.clang_arg(format!("-I{}", inc.display()));
        }
        for dir in &l.link_paths {
            println!("cargo:rustc-link-search={}", dir.display());
        }
        println!("cargo:rustc-link-lib={lib}");
    }
    build.compile("wayrdp-shim");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings = bindgen
        .allowlist_function("freerdp_.*")
        .allowlist_function("wayrdp_.*")
        .allowlist_function("WTS.*")
        .allowlist_function("rdpgfx_.*")
        .allowlist_function("disp_.*")
        .allowlist_type("rdp.*")
        .allowlist_type("freerdp.*")
        .allowlist_type("RdpGfx.*")
        .allowlist_type("RDPGFX_.*")
        .allowlist_type("DispServer.*")
        .allowlist_var("RDP.*")
        .allowlist_var("KBD.*")
        .allowlist_var("PTR_.*")
        // Settings key ids (DesktopWidth, RdpServerRsaKey, TlsSecurity, ...)
        // and the KBD_/PTR_ input-flag macros the peer thread decodes.
        .allowlist_var("FreeRDP_.*")
        .allowlist_var("KBD_FLAGS_.*")
        .allowlist_var("PTR_FLAGS_.*")
        .derive_default(true)
        .generate()
        .expect("bindgen failed against FreeRDP 3 headers");
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("could not write bindings.rs");
}
