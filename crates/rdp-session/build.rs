//! Builds the C layer against libfreerdp 3 from `FREERDP_PREFIX` (default
//! `~/.local/opt/freerdp-rmc`, where `tools/freerdp/build.sh --install` puts it).
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let prefix = std::env::var("FREERDP_PREFIX").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/opt/freerdp-rmc")
    });
    println!("cargo:rerun-if-env-changed=FREERDP_PREFIX");
    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-changed=csrc/shim.h");
    println!("cargo:rerun-if-changed=csrc/aadtoken.c");
    println!("cargo:rerun-if-changed=csrc/aadtoken.h");
    println!("cargo:rerun-if-changed=csrc/secret.c");
    println!("cargo:rerun-if-changed=csrc/secret.h");
    let pc_path = format!("{prefix}/lib64/pkgconfig");
    let out = Command::new("pkg-config")
        .env("PKG_CONFIG_PATH", &pc_path)
        .args(["--cflags", "freerdp-client3", "freerdp3", "winpr3"])
        .output()
        .expect("pkg-config not found");
    assert!(
        out.status.success(),
        "pkg-config found no FreeRDP 3 in {pc_path}"
    );
    let mut build = cc::Build::new();
    build
        .file("csrc/shim.c")
        .file("csrc/aadtoken.c")
        .file("csrc/secret.c")
        .include("csrc")
        .warnings(true)
        .flag_if_supported("-std=gnu11")
        .flag_if_supported("-Wno-deprecated-declarations");
    for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-I") {
            build.include(PathBuf::from(dir));
        } else {
            build.flag(flag);
        }
    }
    build.compile("rdpc_shim");
    let lib = format!("{prefix}/lib64");
    println!("cargo:rustc-link-search=native={lib}");
    for l in ["freerdp-client3", "freerdp3", "winpr3"] {
        println!("cargo:rustc-link-lib=dylib={l}");
    }
    // Dependent binaries read this (DEP_RDPC_SHIM_LIBDIR) to set their rpath.
    println!("cargo:libdir={lib}");
}
