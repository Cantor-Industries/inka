# Verifying a published release

Run this after the tag-triggered release workflow publishes a GitHub Release, to
confirm the artifacts install and run on a clean machine.

## 1. What success looks like

A GitHub Release exists for the tag (`v0.1.1`, …) with these assets:

- `inka_<v>_amd64.deb` — Linux toolchain (CLI + launcher + patcher + curated patches)
- `libinka_runtime-<deno>.so` — the shared runtime tuple
- `libinka_resolver-<v>.so` — the resolution engine
- `store.tar.gz` + `seed-manifest.json` — the default-store snapshot record
- `.sha256` sidecars for the above, and `versions.json`

`versions.json` records the tag's `release`, `deno_runtime`, `resolver`, and the
runtime file's `runtime_sha256` — the store identity `inka doctor` prints should
match the store snapshot you seed.

## 2. The release download base

Release asset URLs are:

```
https://github.com/<owner>/<repo>/releases/download/<tag>/<filename>
```

That makes the **download base** for `--from`:

```
https://github.com/<owner>/<repo>/releases/download/<tag>
```

`inka install` and `inka pkg seed` fetch `<base>/<filename>` directly, so point
them at that base (no trailing filename).

## 3. Integrity check (optional)

```sh
cd <download-dir>
for f in inka_*.deb libinka_runtime-*.so libinka_resolver-*.so store.tar.gz; do
  sha256sum -c "$f.sha256"        # sidecars are bare-hex
done
# runtime file matches versions.json:
jq -r .runtime_sha256 versions.json   # == sha256sum of libinka_runtime-*.so
```

## 4. Clean install test

Use a throwaway environment so the host's own runtime/store are never touched:

```sh
export INKA_RUNTIME_HOME="$HOME/.cache/inka-verify/runtime"
export INKA_STORE="$INKA_RUNTIME_HOME/store"
mkdir -p "$INKA_RUNTIME_HOME"
```

1. Install the toolchain:
   ```sh
   sudo apt install ./inka_<v>_amd64.deb
   ```
   (`/usr/bin/inka` resolves to `/usr/lib/inka/inka`, next to the launcher,
   patcher, and `patches/`.)
2. Install the runtime + resolver from the release:
   ```sh
   inka install <deno> --from https://github.com/<owner>/<repo>/releases/download/<tag>
   ```
3. Seed the default store from the same base (GitHub assets are flat):
   ```sh
   inka pkg seed --from https://github.com/<owner>/<repo>/releases/download/<tag>
   ```
4. Health check:
   ```sh
   inka doctor
   ```
   Expect: the installed runtime + resolver (ABI 2) and a **present** store
   (`packages=N`, `sha=` matching the release).
5. Store-mode imports work — write small apps and run them:
   ```sh
   printf 'import { Effect } from "effect"; console.log(typeof Effect.succeed);\n' > e.js
   printf 'import { Hono } from "hono"; const a = new Hono(); console.log(a.routes.length);\n' > h.js
   printf 'import { WebSocket } from "ws"; console.log(typeof WebSocket);\n' > w.js
   inka run -A e.js
   inka run -A h.js
   inka run -A w.js
   ```
6. An artifact builds and runs:
   ```sh
   printf 'console.log("verify-ok");\n' > v.js
   inka build v.js -o v && ./v
   ```
7. Vendored auto-conversion works (uses the installed patcher + `patches/`):
   ```sh
   mkdir scratch && cd scratch
   inka add ms
   ls vendored/ms/esm.js        # proves the CJS→ESM conversion ran
   ```

If every step above passes, the release is good to promote.

## 5. When it fails

| Symptom | Likely cause / action |
|---|---|
| No release created for the tag | The workflow failed before publish — open the Actions run; it fails at smoke if any check trips (store imports, artifact, vendored conversion) or at publish if a release already exists for the tag |
| `inka doctor` shows no store | Run `inka pkg seed --from <base>` (flat GitHub assets aren't auto-seeded by `inka install`, which only auto-seeds a `store/` subdir layout) |
| `doctor` resolver warning / ABI mismatch | Re-run `inka install <deno> --from <base>`; confirm the resolver asset is present in the release |
| `NotCapable` / permission errors | Deny-by-default — add `-A`, `-P`, or granular `--allow-*` flags (see [Permissions](permissions.md)) |
| `pkg seed` network errors | The store snapshot step needs npm + registry access at *build* time; seeding needs network at *install* time |

## Related

- [Deployment](deployment.md) — the release CI pipeline and distribution model
- [Getting started](getting-started.md) — first-time install and run
- [Troubleshooting](troubleshooting.md) — `doctor`, exit codes, common errors
