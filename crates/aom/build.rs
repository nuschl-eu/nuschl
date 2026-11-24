// SPDX-License-Identifier: EUPL-1.2
//! Bindings to the part of libaom's encoder API that `ffi.rs` calls,
//! generated from the headers pkg-config names for `aom`.

use std::{env, path::PathBuf};

fn main() {
    // Static: libaom.so.3 may be in the process already, from PipeWire's
    // FFmpeg plugin, with the decoder this build leaves out.
    let library = pkg_config::Config::new()
        .atleast_version("3.8")
        .statik(true)
        .probe("aom")
        .expect("libaom (pkg-config aom)");
    bindgen::Builder::default()
        .header_contents(
            "aom.h",
            "#include <aom/aom_encoder.h>\n#include <aom/aomcx.h>\n",
        )
        .clang_args(
            library
                .include_paths
                .iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .allowlist_function(
            "aom_codec_(av1_cx|enc_config_default|enc_init_ver|control|encode|get_cx_data|destroy)|aom_img_wrap",
        )
        .allowlist_type("aom_svc_(params|layer_id|ref_frame_config)_t|aome_enc_control_id|aom_tune_content")
        .allowlist_var("AOM_(USAGE_REALTIME|ENCODER_ABI_VERSION|EFLAG_FORCE_KF)")
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .derive_default(true)
        .layout_tests(false)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .generate()
        .expect("libaom bindings")
        .write_to_file(PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("aom.rs"))
        .expect("write the libaom bindings");
}
