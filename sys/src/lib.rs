#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

#[cfg(not(any(doc, feature = "docsrs")))]
include!(concat!(env!("OUT_DIR"), "/vpx.rs"));

#[cfg(any(doc, feature = "docsrs"))]
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/vpx.rs"));
