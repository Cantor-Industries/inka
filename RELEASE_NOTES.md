# inka release notes

> Maintained by hand and used verbatim as the GitHub Release body
> (`body_path: RELEASE_NOTES.md` in `.github/workflows/release.yml`). Update it
> for each release; it is not templated, so write the concrete version numbers
> and the full install URL.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Assets

- `inka-toolchain-<release>-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-<tuple>.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.
