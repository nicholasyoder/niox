//! Finds libroc 0.4 with pkg-config and generates its bindings with
//! bindgen (Roc has no official Rust bindings). Needs Fedora's
//! `roc-toolkit-devel` (Debian: `libroc-dev`) and libclang.

use std::env;
use std::path::PathBuf;

fn main() {
    // 0.5 reworks the C API, so only 0.4.x is accepted.
    let roc = match pkg_config::Config::new()
        .range_version("0.4".."0.5")
        .probe("roc")
    {
        Ok(lib) => lib,
        Err(err) => panic!("libroc 0.4 not found (install roc-toolkit-devel): {err}"),
    };

    let bindings = bindgen::Builder::default()
        .header_contents(
            "wrapper.h",
            "#include <roc/context.h>\n\
             #include <roc/log.h>\n\
             #include <roc/sender_encoder.h>\n\
             #include <roc/receiver_decoder.h>\n",
        )
        .clang_args(
            roc.include_paths
                .iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .allowlist_function("roc_.*")
        .allowlist_type("roc_.*")
        .allowlist_var("ROC_.*")
        // Constants named as in C (ROC_INTERFACE_AUDIO_SOURCE), not
        // prefixed with their enum's name.
        .prepend_enum_name(false)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate();
    let bindings = match bindings {
        Ok(bindings) => bindings,
        Err(err) => panic!("generating libroc bindings: {err}"),
    };

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default()).join("bindings.rs");
    if let Err(err) = bindings.write_to_file(&out) {
        panic!("writing {}: {err}", out.display());
    }
}
