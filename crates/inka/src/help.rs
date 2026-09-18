// inka CLI help: declarative definitions rendered short (`-h`) or long
// (`--help`). Keeping the text here means every command renders the same way,
// and `help <command>` works without per-command plumbing.

use deno_terminal::colors;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Short,
    Long,
}

/// One command's help. Empty sections are omitted.
pub(crate) struct Help {
    /// Command name (`""` for the top level).
    pub name: &'static str,
    pub about: &'static str,
    /// Usage lines, without the leading `inka `.
    pub usage: &'static [&'static str],
    pub commands: &'static [(&'static str, &'static str)],
    pub arguments: &'static [(&'static str, &'static str)],
    pub options: &'static [(&'static str, &'static str)],
    /// Long help only.
    pub permissions: &'static [(&'static str, &'static str)],
    pub examples: &'static [&'static str],
    pub env: &'static [(&'static str, &'static str)],
}

pub(crate) fn print(help: &Help, mode: Mode) {
    print!("{}", render(help, mode));
}

/// Render a help screen (pure, so it can be tested).
pub(crate) fn render(h: &Help, mode: Mode) -> String {
    let mut out = String::new();
    out.push('\n');
    if h.name.is_empty() {
        out.push_str(&format!(
            "{} {}\n",
            colors::bold("inka"),
            colors::gray(env!("CARGO_PKG_VERSION"))
        ));
    } else {
        out.push_str(&format!("{}\n", colors::bold(format!("inka {}", h.name))));
    }
    if !h.about.is_empty() {
        out.push('\n');
        out.push_str(&format!("{}\n", h.about));
    }

    out.push('\n');
    out.push_str(&format!("{}\n", colors::bold("Usage:")));
    for u in h.usage {
        out.push_str(&format!("  inka {u}\n"));
    }

    section(&mut out, "Commands", h.commands);
    section(&mut out, "Arguments", h.arguments);
    section(&mut out, "Options", h.options);

    if mode == Mode::Long {
        section(&mut out, "Permissions", h.permissions);
        if !h.examples.is_empty() {
            out.push('\n');
            out.push_str(&format!("{}\n", colors::bold("Examples:")));
            for ex in h.examples {
                out.push_str(&format!("  {ex}\n"));
            }
        }
        section(&mut out, "Environment", h.env);
    }
    out
}

fn section(out: &mut String, title: &str, items: &[(&str, &str)]) {
    if items.is_empty() {
        return;
    }
    out.push('\n');
    out.push_str(&format!("{}:\n", colors::bold(title)));
    if !items.iter().any(|(l, _)| !l.is_empty()) {
        // Prose block (e.g. a Permissions note).
        for &(_, desc) in items {
            out.push_str(&format!("  {desc}\n"));
        }
        return;
    }
    let width = items
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);
    for &(label, desc) in items {
        let padded = format!("{label:<width$}");
        out.push_str(&format!("  {}  {desc}\n", colors::bold(padded)));
    }
}

/// Closest candidate by edit distance (≤ 2), for "did you mean" hints.
pub(crate) fn suggest<'a>(input: &str, candidates: &'a [&str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (levenshtein(input, c), *c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub(crate) fn top() -> &'static Help {
    &Help {
        name: "",
        about: "Build one-file JavaScript/TypeScript executables on a shared Deno runtime.",
        usage: &["<command> [options]"],
        commands: &[
            ("build", "bundle an entry into a self-contained executable"),
            (
                "run",
                "execute a .ts/.js file through the installed runtime",
            ),
            ("cache", "fetch remote modules into the Deno cache"),
            ("update", "update the toolchain and shared runtime"),
            ("doctor", "diagnose the machine, or inspect an executable"),
            ("help", "show help for a command"),
        ],
        arguments: &[],
        options: &[
            ("-h, --help", "show this help (or `inka <command> --help`)"),
            ("-V, --version", "print the inka toolchain version"),
        ],
        permissions: &[],
        examples: &[
            "inka run app.ts          iterate on a script",
            "inka build app.ts        -> ./app",
            "inka cache app.ts        fetch jsr:/remote deps",
            "inka doctor ./app        inspect a built artifact",
            "inka update              update toolchain + runtime",
        ],
        env: &[
            (
                "INKA_RUNTIME_HOME",
                "override the per-user runtime directory",
            ),
            ("DENO_DIR", "Deno cache for jsr:/remote (must be absolute)"),
            ("INKA_LAUNCHER", "path to inka-launcher for `build`"),
            ("INK_LOG", "log level: error|warn|info|debug|trace"),
            ("INK_LOG_STYLE", "color: auto (default), always, never"),
        ],
    }
}

pub(crate) fn build() -> &'static Help {
    &Help {
        name: "build",
        about: "bundle an entry and its dependencies into a self-contained executable",
        usage: &["build [source] [options]"],
        commands: &[],
        arguments: &[("source", "entry file (or use -s/--source)")],
        options: &[
            (
                "-s, --source <file>",
                "source file (default: the positional argument)",
            ),
            (
                "-o, --output <file>",
                "output executable (default: source without its extension)",
            ),
            (
                "--runtime <spec>",
                "runtime requirement, e.g. '>=0.266.7' (overrides config)",
            ),
            (
                "--tested-against <ver>",
                "never roll forward past this runtime",
            ),
            (
                "-A, --allow-all",
                "bake permissions=all (trimmed by --deny-*)",
            ),
            (
                "-R, -W, -N, -E, -S",
                "bake read/write/net/env/sys; -R=<list> scopes it",
            ),
            (
                "--allow-<cat>[=list]",
                "bake a grant for read|write|net|env|run|sys|ffi|import",
            ),
            ("--deny-<cat>[=list]", "deny within an allowed category"),
            (
                "-P[=<set>]",
                "bake a named config permission set (bare -P = `default`)",
            ),
            ("--minify", "minify the bundle"),
            ("--sourcemap", "embed an inline source map"),
            (
                "--external <pkg>",
                "leave a package unbundled and embed it from node_modules",
            ),
            (
                "--embed-dir",
                "also embed the current directory tree (assets)",
            ),
            (
                "--path-base <exe|cwd>",
                "anchor relative read/write grants (default cwd)",
            ),
            ("--fetch", "fetch jsr:/remote deps into the Deno cache"),
            ("-h, --help", "show this help"),
        ],
        permissions: &[
            ("", "Deny-by-default. Grants bake from CLI flags, deno.json"),
            ("", "compile.permissions, or an inka.permissions marker."),
        ],
        examples: &[
            "inka build app.ts",
            "inka build -A app.ts",
            "inka build --external sharp app.ts",
        ],
        env: &[],
    }
}

pub(crate) fn run() -> &'static Help {
    &Help {
        name: "run",
        about: "execute a .ts/.js file through the installed runtime",
        usage: &["run [options] <file> [args...]"],
        commands: &[],
        arguments: &[
            ("<file>", "entry file to execute"),
            (
                "[args...]",
                "arguments passed to the program (after <file> or `--`)",
            ),
        ],
        options: &[
            ("-A, --allow-all", "allow everything (trimmed by --deny-*)"),
            ("-R, -W, -N, -E, -S[=list]", "allow read/write/net/env/sys"),
            (
                "--allow-<cat>[=list]",
                "grant read|write|net|env|run|sys|ffi|import",
            ),
            ("--deny-<cat>[=list]", "deny within an allowed category"),
            (
                "-P[=<name>]",
                "apply a named permission set from the config",
            ),
            ("--runtime <ver>", "use a specific installed runtime tuple"),
            (
                "--path-base <exe|cwd>",
                "anchor relative read/write grants (default cwd)",
            ),
            ("--fetch", "fetch missing jsr:/remote deps before running"),
            ("--", "end of options (the file may start with '-')"),
            ("-h, --help", "show this help"),
        ],
        permissions: &[
            (
                "",
                "Deny-by-default; no prompting. `--deny-*` needs an allow",
            ),
            ("", "source (-A or --allow-*)."),
        ],
        examples: &[
            "inka run app.ts",
            "inka run -A app.ts",
            "inka run -R=./data --allow-net app.ts",
        ],
        env: &[],
    }
}

/// Fetch remote modules into the Deno cache (opt-in network).
pub(crate) fn cache() -> &'static Help {
    &Help {
        name: "cache",
        about: "fetch remote (jsr:/https:) modules into the Deno cache",
        usage: &["cache <file>"],
        commands: &[],
        arguments: &[("<file>", "entry file whose import graph to fetch")],
        options: &[("-h, --help", "show this help")],
        permissions: &[],
        examples: &[
            "inka cache app.ts       fetch jsr:/remote deps",
            "inka cache -h",
        ],
        env: &[],
    }
}

pub(crate) fn update() -> &'static Help {
    &Help {
        name: "update",
        about: "update the toolchain and shared runtime",
        usage: &["update [<version>] [options]"],
        commands: &[],
        arguments: &[(
            "<version>",
            "install that exact runtime tuple (offline/pinned)",
        )],
        options: &[
            (
                "--from <dir-or-url>",
                "release base override (mirror or local dir)",
            ),
            (
                "--sha256 <hex>",
                "expected checksum for the downloaded runtime",
            ),
            ("--insecure", "skip checksum verification"),
            ("--home <dir>", "runtime install directory"),
            ("--no-toolchain", "do not touch the toolchain"),
            ("--toolchain-only", "only update the toolchain"),
            ("--no-runtime", "skip the runtime"),
            ("-h, --help", "show this help"),
        ],
        permissions: &[],
        examples: &["inka update", "inka update 0.266.7 --from <base>"],
        env: &[],
    }
}

pub(crate) fn doctor() -> &'static Help {
    &Help {
        name: "doctor",
        about: "diagnose the machine, or inspect an inka executable",
        usage: &["doctor [artifact]"],
        commands: &[],
        arguments: &[(
            "artifact",
            "an inka executable to inspect instead of the machine",
        )],
        options: &[
            ("--json", "machine-readable output (artifact mode)"),
            ("-h, --help", "show this help"),
        ],
        permissions: &[],
        examples: &["inka doctor", "inka doctor ./app"],
        env: &[],
    }
}

pub(crate) fn help() -> &'static Help {
    &Help {
        name: "help",
        about: "show help for a command",
        usage: &["help [command]"],
        commands: &[],
        arguments: &[(
            "[command]",
            "command to describe (build, run, cache, update, doctor)",
        )],
        options: &[],
        permissions: &[],
        examples: &["inka help build"],
        env: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_help_lists_commands() {
        let text = render(top(), Mode::Long);
        for cmd in ["build", "run", "cache", "update", "doctor", "help"] {
            assert!(text.contains(cmd), "missing {cmd}:\n{text}");
        }
        assert!(!text.contains("list"), "list must be gone:\n{text}");
    }

    #[test]
    fn long_has_examples_short_does_not() {
        let long = render(build(), Mode::Long);
        let short = render(build(), Mode::Short);
        assert!(long.contains("Examples:"), "{long}");
        assert!(!short.contains("Examples:"), "{short}");
        assert!(short.contains("--minify"), "{short}");
    }

    #[test]
    fn suggests_close_commands() {
        assert_eq!(suggest("buld", &["build", "run", "doctor"]), Some("build"));
        assert_eq!(suggest("runn", &["build", "run"]), Some("run"));
        assert_eq!(suggest("xyz", &["build", "run"]), None);
    }
}
