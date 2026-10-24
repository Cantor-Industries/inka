// Copyright 2018-2026 the Deno authors. MIT license.

use std::env;
use std::path::PathBuf;

fn main() {
  let target = env::var("TARGET").unwrap();
  println!("cargo:rustc-env=TARGET={target}");

  // Compile llhttp (Node.js HTTP/1.1 parser)
  let llhttp_dir = PathBuf::from("ops/llhttp/c");
  let mut build = cc::Build::new();
  build
    .files([
      llhttp_dir.join("llhttp.c"),
      llhttp_dir.join("http.c"),
      llhttp_dir.join("api.c"),
    ])
    .include(&llhttp_dir)
    .warnings(false);
  // MSVC's cl does not understand GCC's -std=c99 and warns (D9002) before
  // compiling in its default C mode.
  if !target.contains("msvc") {
    build.std("c99");
  }
  build.compile("llhttp");
}
