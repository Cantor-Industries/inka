# Verifying a published release

Run this after the tag-triggered release workflow publishes a GitHub Release, to
confirm the artifacts install and run on a clean machine.

## 1. What success looks like

A GitHub Release exists for the tag (`v0.5.0`, …) with these assets:

- `inka-toolchain-<rel>-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher;
- `libinka_runtime-<runtime>.so` — the shared runtime tuple;
- `install.sh` — the bootstrap installer;
- `.sha256` sidecars for the above, and `versions.json`.

`versions.json` records the release, a `toolchain` block (version/target/archive/
sha256), the `runtime` tuple + its `deno_runtime` base, and the runtime `sha256`
— `inka doctor` prints the installed identities, which should match.

## 2. The release download base

Release asset URLs are:

```
https://github.com/<owner>/<repo>/releases/download/<tag>/<filename>
```

That makes the **download base** for `--from`:

```
https://github.com/<owner>/<repo>/releases/download/<tag>
```

`install.sh --from <base>` and `inka update --from <base>` fetch
`<base>/versions.json` and the referenced assets directly, so point them at that
base (no trailing filename).

## 3. Integrity check (optional)

```sh
cd <download-dir>
for f in inka-toolchain-*.tar.gz libinka_runtime-*.so; do
  sha256sum -c "$f.sha256"        # sidecars are bare-hex
done
# runtime file matches versions.json:
jq -r .runtime_sha256 versions.json   # == sha256sum of libinka_runtime-*.so
```

## 4. Clean install test

Use a throwaway prefix and engine dir so the host's own install is never
touched:

```sh
export INKA_RUNTIME_HOME="$HOME/.cache/inka-verify/runtime"
mkdir -p "$INKA_RUNTIME_HOME"
```

1. Install from the release:
   ```sh
   ./install.sh --from <download-dir-or-url> --yes --prefix "$HOME/.cache/inka-verify/prefix"
   ```
2. Health check:
   ```sh
   "$HOME/.cache/inka-verify/prefix/bin/inka" doctor
   ```
   Expect: the installed runtime and `bundling: available`, with no warnings.
3. An artifact builds and runs (self-contained):
   ```sh
   printf 'console.log("verify-ok");\n' > v.js
   inka build v.js -o v && ./v
   ```
4. An `--external` package is embedded:
   ```sh
   mkdir npm && cd npm && npm install --no-save ms@2.1.3
   printf 'import ms from "ms";\nconsole.log(ms(60000));\n' > m.js
   inka build m.js -o m --external ms
   mv node_modules node_modules.hidden && ./m && mv node_modules.hidden node_modules
   ```
5. Permissions are deny-by-default and bake from config:
   ```sh
   printf 'try { Deno.readTextFileSync("x"); console.log("allow"); } catch { console.log("denied"); }\n' > p.js
   inka run p.js            # -> denied
   ```
6. Re-running the installer/`inka update` is a no-op (already current):
   ```sh
   ./install.sh --from <base> --yes --prefix "$HOME/.cache/inka-verify/prefix"
   inka update --from <base>    # -> "is current" for toolchain/runtime
   ```

If every step above passes, the release is good to promote.

## 5. When it fails

| Symptom | Likely cause / action |
|---|---|
| No release created for the tag | The workflow failed before publish — open the Actions run; it fails at smoke if any check trips or at publish if a release already exists for the tag |
| `install.sh` checksum error | A sidecar or `versions.json` `sha256` doesn't match the asset — re-run the workflow |
| `doctor` shows no runtime / exit 3 | Re-run `inka update --from <base>`; confirm the runtime asset is present |
| `inka build` says "without bundling support" | The toolchain wasn't built with `--features bundle`; re-run the workflow |
| `NotCapable` / permission errors | Deny-by-default — add `-A`, `-P`, or granular `--allow-*` flags (see [Permissions](permissions.md)) |

## Related

- [Deployment](deployment.md) — the release CI pipeline and distribution model
- [Install & upgrade](install-and-upgrade.md) — first-time install and upgrade
- [Troubleshooting](troubleshooting.md) — `doctor`, exit codes, common errors
