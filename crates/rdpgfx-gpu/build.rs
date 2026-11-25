//! Compiles the GLSL compute shaders to SPIR-V with glslc (shaderc), or with
//! glslangValidator where glslc is not available (includes are inlined for it).
use std::path::{Path, PathBuf};
use std::process::Command;

const SHADERS: &[&str] = &[
    "avc444_main",
    "avc444_aux",
    "yuv444_to_rgba",
    "fill",
    "present",
    "overlay",
];

/// Replaces `#include "x"` lines with the file contents (one level, like the shaders use).
fn inline_includes(src: &Path) -> String {
    let dir = src.parent().expect("shader dir");
    std::fs::read_to_string(src)
        .expect("read shader")
        .lines()
        .map(|l| match l.trim().strip_prefix("#include \"") {
            Some(rest) => {
                let name = rest.trim_end_matches('"');
                std::fs::read_to_string(dir.join(name)).expect("read include")
            }
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shaders/common.glsl");
    let have_glslc = Command::new("glslc").arg("--version").output().is_ok();
    for name in SHADERS {
        let src = format!("shaders/{name}.comp");
        println!("cargo:rerun-if-changed={src}");
        let spv = out.join(format!("{name}.spv"));
        let status = if have_glslc {
            Command::new("glslc")
                .args(["--target-env=vulkan1.2", "-O", "-Werror", &src, "-o"])
                .arg(&spv)
                .status()
                .expect("glslc")
        } else {
            let flat = out.join(format!("{name}.comp"));
            std::fs::write(&flat, inline_includes(Path::new(&src))).expect("write shader");
            Command::new("glslangValidator")
                .args(["-V", "--target-env", "vulkan1.2", "-o"])
                .arg(&spv)
                .arg(&flat)
                .status()
                .expect("neither glslc nor glslangValidator found (dnf install glslc)")
        };
        assert!(status.success(), "shader compiler failed for {src}");
    }
}
