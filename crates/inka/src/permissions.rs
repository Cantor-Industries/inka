// Shared permission flag parsing and DSL rendering for `inka build` and
// `inka run`.
//
// Both commands accept the same permission surface:
//   -A / --allow-all                     everything (trimmed by --deny-*)
//   -R -W -N -E -S[=list]                deno short forms (read/write/net/
//                                        env/sys) + long --allow-<cat>
//   -P / -P=<name> / --permission-set[=<name>]
//                                        a named config permission set
//                                        (bare -P = the config `default` set)
//   --allow-<cat>[=list]                 grant category read|write|net|env|run|sys|ffi
//   --deny-<cat>[=list]                  trim an allowed category
//
// `run` applies the rendered DSL for one invocation; `build` bakes it into the
// artifact, overriding any config-sourced permission lines.

use std::path::Path;

pub(crate) const CATEGORIES: [&str; 7] = ["read", "write", "net", "env", "run", "sys", "ffi"];

/// Parsed permission flags, shared by `build` and `run`.
#[derive(Default)]
pub(crate) struct Flags {
    pub allow_all: bool,
    pub permset: Option<String>,
    pub allow: Vec<(String, String)>, // (category, list or "*")
    pub deny: Vec<(String, String)>,
}

impl Flags {
    /// True when the user selected a permission source on the CLI (`--deny-*`
    /// alone is not a selection).
    pub fn selects(&self) -> bool {
        self.allow_all || self.permset.is_some() || !self.allow.is_empty()
    }
}

/// The result of interpreting one CLI token as a permission flag.
pub(crate) enum PermFlag {
    /// Not a permission flag.
    Not,
    /// Consumed the token.
    Once,
    /// `--permission-set` without `=`: the caller must consume the next token.
    ConsumeNext,
}

pub(crate) fn cat_for_short(short: char) -> Option<&'static str> {
    match short {
        'R' => Some("read"),
        'W' => Some("write"),
        'N' => Some("net"),
        'E' => Some("env"),
        'S' => Some("sys"),
        _ => None,
    }
}

/// A raw newline in a CLI value would inject an extra manifest/DSL line.
fn reject_newline(what: &str, value: &str) -> Result<(), String> {
    if value.contains('\n') || value.contains('\r') {
        return Err(format!("{what} contains a newline, which is not allowed"));
    }
    Ok(())
}

/// Interpret `arg` as a permission flag, mutating `flags`. Returns `Not` when it
/// is some other option.
pub(crate) fn parse_perm_flag(flags: &mut Flags, arg: &str) -> Result<PermFlag, String> {
    match arg {
        "-A" | "--allow-all" => {
            flags.allow_all = true;
            Ok(PermFlag::Once)
        }
        "-P" => {
            flags.permset = Some("default".to_string());
            Ok(PermFlag::Once)
        }
        "--permission-set" => Ok(PermFlag::ConsumeNext),
        _ if arg.starts_with("-P=") => {
            let name = arg["-P=".len()..].to_string();
            reject_newline("permission set name", &name)?;
            flags.permset = Some(name);
            Ok(PermFlag::Once)
        }
        _ if arg.starts_with("--permission-set=") => {
            let name = arg["--permission-set=".len()..].to_string();
            reject_newline("permission set name", &name)?;
            flags.permset = Some(name);
            Ok(PermFlag::Once)
        }
        _ if arg.starts_with("--allow-") || arg.starts_with("--deny-") => {
            let deny = arg.starts_with("--deny-");
            let prefix = if deny { "--deny-" } else { "--allow-" };
            let body = &arg[prefix.len()..];
            let (cat, list) = match body.split_once('=') {
                Some((c, v)) => (c.to_string(), v.to_string()),
                None => (body.to_string(), "*".to_string()),
            };
            if !CATEGORIES.contains(&cat.as_str()) {
                return Err(format!(
                    "unknown permission category '{cat}' (expected one of {})",
                    CATEGORIES.join(", ")
                ));
            }
            let list = if list.is_empty() {
                "*".to_string()
            } else {
                list
            };
            reject_newline(
                &format!("--{}-{cat} value", if deny { "deny" } else { "allow" }),
                &list,
            )?;
            let slot = if deny {
                &mut flags.deny
            } else {
                &mut flags.allow
            };
            slot.push((cat, list));
            Ok(PermFlag::Once)
        }
        _ if arg.len() >= 2 && cat_for_short(arg.as_bytes()[1] as char).is_some() => {
            let short = arg.as_bytes()[1] as char;
            let cat = cat_for_short(short).unwrap().to_string();
            let body = &arg[2..];
            let list = if body.is_empty() {
                "*".to_string()
            } else if let Some(v) = body.strip_prefix('=') {
                if v.is_empty() {
                    "*".to_string()
                } else {
                    v.to_string()
                }
            } else {
                return Err(format!(
                    "option '{arg}' takes an optional '=<list>' value (e.g. -{short}=./data)"
                ));
            };
            reject_newline(&format!("-{short} value"), &list)?;
            flags.allow.push((cat, list));
            Ok(PermFlag::Once)
        }
        _ => Ok(PermFlag::Not),
    }
}

/// Merge repeated per-category entries: `*` wins over lists; explicit lists are
/// joined with commas (one DSL line per category).
pub(crate) fn merge_cat(entries: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (cat, list) in entries {
        match out.iter_mut().find(|(c, _)| c == cat) {
            Some((_, cur)) => {
                if list == "*" {
                    *cur = "*".to_string();
                } else if cur != "*" {
                    if !cur.is_empty() {
                        cur.push(',');
                    }
                    cur.push_str(list);
                }
            }
            None => out.push((cat.clone(), list.clone())),
        }
    }
    out
}

/// Reject mutually-exclusive or ineffective selections:
///  - `-A` with `-P`/`--allow-*` (a `--deny-*` may trim `-A`);
///  - `-P` with granular flags;
///  - `--deny-*` alone, with no allow source to trim (it would otherwise be
///    silently ignored, or — when config supplies a grant — silently dropped).
pub(crate) fn validate(flags: &Flags) -> Result<(), String> {
    if flags.allow_all && (flags.permset.is_some() || !flags.allow.is_empty()) {
        return Err("--allow-all cannot be combined with -P/--permission-set or --allow-*".into());
    }
    if flags.permset.is_some() && (!flags.allow.is_empty() || !flags.deny.is_empty()) {
        return Err("-P/--permission-set cannot be combined with --allow-*/--deny-*".into());
    }
    if !flags.deny.is_empty() && !flags.allow_all && flags.allow.is_empty() {
        return Err(
            "--deny-* needs an allow source (-A or --allow-*); a bare deny would be ignored".into(),
        );
    }
    Ok(())
}

/// Render the permission DSL from parsed flags. `-P` resolves a named set from
/// `root`'s config; returns any notes (e.g. an unknown set name). Errors on a
/// value the manifest/DSL cannot represent (a raw newline).
pub(crate) fn dsl(root: &Path, flags: &Flags) -> Result<(String, Vec<String>), String> {
    if flags.allow_all {
        let mut lines = vec!["permissions=all".to_string()];
        for (cat, list) in merge_cat(&flags.deny) {
            lines.push(format!("deny-{cat}={list}"));
        }
        return Ok((lines.join("\n"), Vec::new()));
    }
    if let Some(name) = &flags.permset {
        return crate::config::permission_set_dsl(root, name);
    }
    let mut lines = Vec::new();
    for (cat, list) in merge_cat(&flags.allow) {
        lines.push(format!("allow-{cat}={list}"));
    }
    for (cat, list) in merge_cat(&flags.deny) {
        lines.push(format!("deny-{cat}={list}"));
    }
    Ok((lines.join("\n"), Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> (Flags, Vec<String>) {
        let mut f = Flags::default();
        let mut errs = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match parse_perm_flag(&mut f, args[i]) {
                Ok(PermFlag::Once) => {}
                Ok(PermFlag::ConsumeNext) => {
                    i += 1;
                    f.permset = Some(args[i].to_string());
                }
                Ok(PermFlag::Not) => {}
                Err(e) => errs.push(e),
            }
            i += 1;
        }
        (f, errs)
    }

    #[test]
    fn short_and_long_forms_render_identically() {
        let (f, _) = parse(&["-R", "-W", "-N", "-E", "-S"]);
        let cats: Vec<&str> = f.allow.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(cats, vec!["read", "write", "net", "env", "sys"]);
        let dsl = dsl(Path::new("."), &f).unwrap().0;
        assert!(dsl.contains("allow-read=*"), "{dsl}");
        assert!(dsl.contains("allow-net=*"), "{dsl}");
    }

    #[test]
    fn short_with_list_and_long_allow() {
        let (f, _) = parse(&["-R=./data", "--allow-net=api.example.com"]);
        let dsl = dsl(Path::new("."), &f).unwrap().0;
        assert!(dsl.contains("allow-read=./data"), "{dsl}");
        assert!(dsl.contains("allow-net=api.example.com"), "{dsl}");
    }

    #[test]
    fn bare_p_is_default_set_and_p_equals_names() {
        let (f, _) = parse(&["-P"]);
        assert_eq!(f.permset.as_deref(), Some("default"));
        let (f, _) = parse(&["-P=server"]);
        assert_eq!(f.permset.as_deref(), Some("server"));
        let (f, _) = parse(&["--permission-set", "server"]);
        assert_eq!(f.permset.as_deref(), Some("server"));
    }

    #[test]
    fn allow_all_with_deny() {
        let (f, _) = parse(&["-A", "--deny-read=./secret"]);
        assert_eq!(
            dsl(Path::new("."), &f).unwrap().0,
            "permissions=all\ndeny-read=./secret"
        );
    }

    #[test]
    fn unknown_category_is_an_error() {
        let (_, errs) = parse(&["--allow-bogus"]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("unknown permission category"),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn newline_in_value_is_rejected() {
        let (_, errs) = parse(&["--allow-read=a\nb"]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("newline"), "{}", errs[0]);
        let (_, errs) = parse(&["-P=bad\nname"]);
        assert!(errs.iter().any(|e| e.contains("newline")), "{errs:?}");
    }

    #[test]
    fn validation_rejects_conflicts() {
        let (f, _) = parse(&["-A", "-P"]);
        assert!(validate(&f).is_err());
        let (f, _) = parse(&["-P", "--allow-read"]);
        assert!(validate(&f).is_err());
        let (f, _) = parse(&["-A", "--deny-read=x"]);
        assert!(validate(&f).is_ok());
    }

    #[test]
    fn deny_without_allow_is_rejected() {
        let (f, _) = parse(&["--deny-read=./secret"]);
        let err = validate(&f).expect_err("deny-only must be rejected");
        assert!(err.contains("needs an allow source"), "{err}");
        // An allow source makes the deny valid.
        let (f, _) = parse(&["--allow-read", "--deny-read=./secret"]);
        assert!(validate(&f).is_ok());
        let (f, _) = parse(&["-A", "--deny-read=./secret"]);
        assert!(validate(&f).is_ok());
    }

    #[test]
    fn selects_only_counts_grants() {
        let (f, _) = parse(&["--deny-read=x"]);
        assert!(!f.selects());
        let (f, _) = parse(&["--allow-read"]);
        assert!(f.selects());
    }
}
