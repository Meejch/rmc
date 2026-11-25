//! rpath to the libfreerdp the session crate was built against, and its prefix (for the
//! default path of the AAD login helpers in <prefix>/libexec). Also builds the own login
//! helper `rmc-login` (C, WebKitGTK) next to the client binary.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RMC_RPATH");
    if let Ok(rpath) = std::env::var("RMC_RPATH") {
        // AppImage: bundled libraries next to the binary (DT_RPATH also covers the
        // dependencies of the bundled libraries).
        println!("cargo:rustc-link-arg-bin=rmc=-Wl,--disable-new-dtags,-rpath,{rpath}");
    }
    if let Ok(dir) = std::env::var("DEP_RDPC_SHIM_LIBDIR") {
        if std::env::var_os("RMC_RPATH").is_none() {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
        let prefix = std::path::Path::new(&dir)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!("cargo:rustc-env=RDPC_FREERDP_PREFIX={prefix}");
    }
    build_login_helper();
}

/// Compiles login/rmc-login.c into target/<profile>/rmc-login. Without the
/// WebKitGTK development files only a warning is printed (the FreeRDP helpers still work).
fn build_login_helper() {
    let src = "login/rmc-login.c";
    println!("cargo:rerun-if-changed={src}");
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "webkit2gtk-4.1", "gtk+-3.0"])
        .output();
    let flags = match flags {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            println!("cargo:warning=webkit2gtk-4.1/gtk+-3.0 not found: rmc-login is not built");
            return;
        }
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR = target/<profile>/build/<crate>-<hash>/out (newer Cargo versions add a level:
    // build/<crate>/<hash>/out): the profile directory is the parent of "build".
    let Some(target) = out
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(|b| b.parent())
        .map(PathBuf::from)
    else {
        return;
    };
    let exe = target.join("rmc-login");
    let compiler = cc::Build::new().get_compiler();
    let status = compiler
        .to_command()
        .args(["-O2", "-Wall", "-Wextra", "-o"])
        .arg(&exe)
        .arg(src)
        .args(flags.split_whitespace())
        .status()
        .expect("C compiler");
    if !status.success() {
        panic!("building {src} failed");
    }
    println!("cargo:rustc-env=RDPC_LOGIN_HELPER={}", exe.display());
}
