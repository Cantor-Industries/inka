# Deferred vendored-code patch tracking

These files are derived from upstream code but are not yet represented as a
patch series. They are either heavily diverged, only a subset of a much larger
upstream file, or their exact upstream base is unconfirmed. Track them in the
accompanying GitHub issue; do not treat them as verified patches.

| Unit | Local file | Likely upstream | Why deferred |
|---|---|---|---|
| `hmr` | `crates/inka-runtime/src/hmr.rs` | Deno 2.9.8 `cli/rt/hmr.rs` | Local adaptation diverges heavily (~1.2k diff lines). |
| `desktop_api` | `crates/inka-runtime/src/desktop_api.rs` | Deno 2.9.8 `cli/rt_desktop/lib.rs` | Local file is a subset/adaptation (~3.4k diff lines). |
| `framework` | `crates/inka/src/framework.rs` | Deno 2.9.8 `cli/tools/framework.rs` | Local file is a subset (~2.4k diff lines). |
| `archive` | `crates/inka/src/archive.rs` | Deno `cli/tools/desktop.rs` + `cli/util/extract.rs` | Composite subset (~3.1k diff lines against `extract.rs`). |
| `deep_links` | `crates/inka/src/deep_links.rs` | Deno `cli/tools/desktop.rs` | Subset of an 8k-line file. |
| `ico` | `crates/inka/src/ico.rs` | Deno `cli/tools/desktop.rs` | Single helper carved out of an 8k-line file. |
| `selfextract` | `crates/inka/src/selfextract.rs` | Deno `cli/tools/desktop.rs` | Subset of an 8k-line file. |
| `cef_copy` | `crates/inka/src/cef.rs` | Deno `cli/tools/compile.rs` | Only `copy_entry`/`copy_dir_all` adapted; patch would be mostly delete noise. |
| `windows_msi` | `crates/inka/src/windows_msi.rs` | Deno `cli/tools/desktop.rs` | 1.2k lines, heavily extended beyond Deno. |
| `desktop_devtools` | `crates/inka/src/desktop_devtools.rs` | Deno desktop inspect/devtools mux (source unconfirmed) | 2.4k lines; may be partly inka-original. |
| `desktop_runtime` | `crates/inka-runtime/src/desktop.rs` | Deno `cli/rt_desktop` | Mixed Deno-derived and inka-specific logic. |
| `desktop_cli` | `crates/inka/src/desktop.rs` | Mixed Deno desktop/tooling | Mixed origin; needs audit/splitting. |
| `snapshot_build` | `crates/inka-runtime/build.rs` | libdeno / Deno `cli/snapshot/build.rs` | Exact libdeno base/commit unconfirmed. |
| `desktop_e2e` | `crates/inka-runtime/src/desktop_e2e.rs` | laufey v0.7.0 `examples/native_e2e/src/lib.rs` | Local adaptation ~860 diff lines. |
| `libsui_readers` | `crates/inka-format/src/lib.rs` | libsui 0.16.4 section readers | Confirm copied/adapted vs independently reimplemented. |

## Exit criteria for each deferred unit

1. Exact upstream repo/tag/path and license identified.
2. Either a maintainable patch series or a documented "lineage only" note.
3. Metadata + base/local hashes recorded.
4. `scripts/ci/vendor-patches.sh` coverage if patched.
5. Sync/update and removal conditions documented.
