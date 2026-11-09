//! 1. Embed a manifest that asks for administrator rights (installing a driver needs them).
//! 2. Embed the driver package: if `SOUNDSIGHT_DRIVER_DIR` points at a built package
//!    (the .inf, .sys and .cat from `driver/`), those files are compiled into the exe so
//!    the setup is one file. Without it the exe builds but refuses to install.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOUNDSIGHT_DRIVER_DIR");

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    }

    let mut code = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    if let Ok(dir) = env::var("SOUNDSIGHT_DRIVER_DIR") {
        println!("cargo:rerun-if-changed={dir}");
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("SOUNDSIGHT_DRIVER_DIR={dir}: {e}"))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                matches!(ext.as_str(), "inf" | "sys" | "cat")
            })
            .collect();
        entries.sort();
        let has = |ext: &str| entries.iter().any(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)));
        assert!(has("inf") && has("sys") && has("cat"), "SOUNDSIGHT_DRIVER_DIR={dir} must hold the .inf, .sys and .cat");
        for p in &entries {
            let name = p.file_name().unwrap().to_str().unwrap();
            let abs = fs::canonicalize(p).unwrap();
            code += &format!("    ({name:?}, include_bytes!({:?})),\n", abs.to_str().unwrap());
        }
    }
    code += "];\n";
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("payload.rs");
    fs::write(out, code).unwrap();
}
