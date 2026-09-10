# Verifying a published release

Run this after the tag-triggered release workflow publishes a GitHub Release, to
confirm the artifacts install and run on a clean machine.

## 1. What success looks like

A GitHub Release exists for the tag (`v0.1.1`, …) with these assets:

- `inka_<v>_amd64.deb` — Linux toolchain (CLI + launcher + patcher + curated patches);
  bundles the runtime/store only when they changed since the previous release
- `libinka_runtime-<deno>.so` — the shared runtime tuple
- `libinka_resolver-<v>.so` — the resolution engine
- `store.tar.gz` + `seed-manifest.json` — the default-store snapshot record
- `.sha256` sidecars for the above, and `versions.json`

`versions.json` records the tag's `release`, `deno_runtime`, `resolver`, and the
runtime file's `runtime_sha256`; `seed-manifest.json` records the store's
`sha256` — `inka doctor` prints both identities, which should match.

## 2. The release download base

Release asset URLs are:

```
https://github.com/<owner>/<repo>/releases/download/<tag>/<filename>
```

That makes the **download base** for `--from`:

```
https://github.com/<owner>/<repo>/releases/download/<tag>
```

`inka update --from <base>` fetches `<base>/versions.json` and the referenced
assets directly, so point it at that base (no trailing filename).

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
   patcher, and `patches/`. The `postinst` installs any bundled runtime/store
   system-wide and seeds the installing account's store; the throwaway
   `INKA_RUNTIME_HOME`/`INKA_STORE` above keep the checks below isolated.)
2. Install/refresh the runtime + resolver + store from the release:
   ```sh
   inka update --from https://github.com/<owner>/<repo>/releases/download/<tag>
   ```
   (No version → reads `versions.json`, installs the runtime/resolver, and
   seeds the flat store snapshot. `inka update <deno> --from <base>` installs a
   specific tuple.)
3. Health check:
   ```sh
   inka doctor
   ```
   Expect: the installed runtime + resolver (ABI 2) and a **present** store
   (`packages=N`, `sha=` matching the release).
4. Store-mode imports work — write small apps and run them:
   ```sh
   printf 'import { Effect } from "effect"; console.log(typeof Effect.succeed);\n' > e.js
   printf 'import { Hono } from "hono"; const a = new Hono(); console.log(a.routes.length);\n' > h.js
   printf 'import { WebSocket } from "ws"; console.log(typeof WebSocket);\n' > w.js
   inka run -A e.js
   inka run -A h.js
   inka run -A w.js
   ```
5. An artifact builds and runs:
   ```sh
   printf 'console.log("verify-ok");\n' > v.js
   inka build v.js -o v && ./v
   ```
6. Vendored auto-conversion works (uses the installed patcher + `patches/`):
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
| `inka doctor` shows no store | Run `inka update --from <base>` (reads `seed-manifest.json` + `store.tar.gz` from the flat release assets) |
| `doctor` resolver warning / ABI mismatch | Re-run `inka update --from <base>`; confirm the resolver asset is present in the release |
| `NotCapable` / permission errors | Deny-by-default — add `-A`, `-P`, or granular `--allow-*` flags (see [Permissions](permissions.md)) |
| Store seeding network errors | The store snapshot step needs npm + registry access at *build* time; seeding needs network at *install* time |

## Related

- [Deployment](deployment.md) — the release CI pipeline and distribution model
- [Getting started](getting-started.md) — first-time install and run
- [Troubleshooting](troubleshooting.md) — `doctor`, exit codes, common errors
