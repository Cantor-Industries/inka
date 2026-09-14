<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.5.4`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.266.2`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

A security- and correctness-hardening release. The runtime tuple is unchanged
(no engine rebuild): artifacts and runtimes from 0.5.3 remain compatible.

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Behaviour changes

- **`--deny-*` now requires an allow source.** `inka build`/`run` with a
  `--deny-*` flag but no `-A` and no `--allow-*` is a hard error instead of a
  silent no-op. `-A --deny-*` and `--allow-x --deny-x` still work.
- **Usage errors now exit `2`.** Unknown options, a missing source, and too many
  arguments exit `2`; an explicit help request still exits `0`.
- **Malformed launcher manifest versions are rejected.** A `runtime=` or
  `tested-against=` constraint that cannot be parsed is a hard error (exit `3`),
  not a silently dropped constraint. `>` is now strict greater-than.
- **A mislabeled runtime is refused.** The loaded runtime's reported version must
  match its filename and the manifest constraints (mismatch exits `4`).
- **`INKA_RUNTIME` was removed.** Use the runtime discovered next to the launcher
  (or a release-installed one); the override no longer exists.
- **Empty allow lists are rejected.** `allow-<cat>=` (or an array/join that
  renders empty) is malformed and no longer becomes a global allow.
- **`--sourcemap` emits an inline map**, as the docs always promised.

## Security and hardening

- Permission grants: deny-only builds error (above); config rendering can no
  longer emit an empty `allow-<cat>=` that widens to a global grant.
- Downloads: release/asset names must be plain basenames (no `..`, `/`, `\`,
  absolute paths); toolchain staging uses a fixed filename.
- Transport: `https` fetches pin TLS and redirect protocols (`--proto-redir
  =https`, max 5 redirects; wget `--https-only`). `http://` local mirrors keep
  working. (Publisher signing is not in this release; authenticity lands in a
  later series.)
- Temp files: launcher extraction and `inka update` use unpredictable,
  exclusive, `0700` temp paths and clean up on error/exit — including a runtime's
  own `Deno.exit` path.
- `-o`: refuses a symlinked output, compares canonical identities against the
  source, and writes via a `0755` temp file + atomic `rename`.
- Launcher archive decoder: length fields are bounds-checked (`checked_add` +
  `usize::try_from`); no wrap or panic.
- Manifest embedding: newlines in permission items and `runtime`/
  `tested-against` values are rejected; the runtime spec grammar is validated
  before embedding.
- Dependency embedding: npm dependency and `--external` names are validated and
  every walked path must resolve under the project `node_modules`.
- Config: a non-object `deno.json` named set is authoritative (or rejected); no
  silent `package.json` fallback.
- JSONC: a trailing comma before a comment (`[1, // c\n]`) now parses.

## CI and docs

- CI runs the bundler and `--features bundle` test suites, uses `--locked`, and
  enforces exact Deno pins generically.
- The release workflow splits the untrusted build/smoke job (`contents: read`)
  from the publish job (`contents: write`); release notes come from this file.
- The runtime matrix fails on skipped cases unless explicitly allowed; the
  spike/stub build is real; `smoke.sh` restores `node_modules` on failure.
- The manual was corrected for the flags, exit codes, launcher size, removed
  `INKA_RUNTIME`, workspace `node_modules` climb, and `--external` closure.

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.
