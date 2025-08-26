use std::env;
use std::path::PathBuf;

use bindgen::EnumVariation::Rust;
use pkg_config::Config;

fn main() {
    if env::var("DOCS_RS").is_ok() || env::var("CARGO_DOC").is_ok() {
        return;
    }

    println!("cargo:rerun-if-changed=include/wrapper.h");

    let libs = Config::new().atleast_version("1.15.2").probe("vpx").unwrap();
    let headers = libs.include_paths;

    let mut builder = bindgen::builder().header("include/wrapper.h").default_enum_style(Rust { non_exhaustive: false }).generate_comments(false);

    for header in headers {
        builder = builder.clang_arg("-I").clang_arg(header.to_str().unwrap());
    }

    builder.generate().unwrap().write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("vpx.rs")).unwrap();
}
