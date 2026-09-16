<!--
  Maintainer notes. Everything through this closing `-->` is stripped from the
  published body by the release workflow (`.github/workflows/release.yml`), so
  it is safe to keep internal guidance here.

  The body uses `{{REL}}` (tag minus `v`, e.g. `0.7.1`) and `{{RUNTIME}}`
  (`crates/inka-runtime/runtime-version`, e.g. `0.266.7`) placeholders; the
  workflow substitutes them when staging the release. Do not hardcode versions.
-->
# inka {{REL}} — runtime tuple {{RUNTIME}}

A CLI quality-of-life release. Downloads show a progress bar, every command now
uses one consistent, colored output vocabulary, `inka` with no arguments prints
a proper help screen, and `inka list` is gone (use `inka doctor`). The runtime
tuple moves to `{{RUNTIME}}` (engine messages drop the `[inka]` prefix).

## Upgrade

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
```

Then keep the toolchain and runtime current with `inka update`.

## Breaking changes

- **`inka list` removed.** It duplicated `inka doctor`, which reports the same
  installed runtimes plus project status. Use `inka doctor`.
- **New artifacts require runtime `>={{RUNTIME}}`.** Existing artifacts (floor
  `>=0.266.6`) keep working and roll forward to the new tuple.

## What's new

- **Progress bar.** `inka update` draws a Deno-style bar
  (`Downloading libinka_runtime-…so  [####>------]  45%  12.4MiB/27.1MiB`) while
  fetching the toolchain and runtime. Shown only on a terminal; suppressed by
  `NO_COLOR` or `-q`. `install.sh` shows curl's meter on a terminal.
- **Consistent, styled output.** Every command (`build`, `run`, `update`,
  `doctor`) uses the same sectioned report with `✓`/`!`/`✗`/`→` glyphs.
  Diagnostics are labeled `error:` (red) / `warning:` (yellow) / `hint:` (cyan),
  including multi-line runtime errors and launcher/artifact errors — the
  `[inka]` prefix is gone everywhere. Color follows `NO_COLOR`/`FORCE_COLOR` and
  is on only for a TTY, so piped/CI output stays plain.
- **Help rewrite.** Running `inka` with no arguments prints the full help;
  `-h` is a short summary and `--help` the full one; `inka help <command>` works
  for every command. Unknown commands get a `did you mean` suggestion.
- `inka doctor` is grouped (`Runtimes`, `Project`) and no longer repeats the
  runtime search directories.

## Under the hood

- `inka update` streams artifacts to disk (download, hash, atomic rename)
  instead of buffering the whole runtime `.so` in memory; checksum behavior is
  unchanged.
- New `ui` and `help` modules (backed by `deno_terminal`) for styling,
  verbosity, progress, and declarative help; the launcher uses a tiny built-in
  ANSI helper (no new dependency).

## Assets

- `inka-toolchain-{{REL}}-x86_64-unknown-linux-gnu.tar.gz` — CLI + launcher
- `libinka_runtime-{{RUNTIME}}.so` — shared runtime tuple
- `install.sh` + `versions.json` — bootstrap installer + version record

`.sha256` sidecars are published for the toolchain and runtime.
