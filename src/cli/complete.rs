//! Shell completion. `desk completions <shell>` prints a script that calls `desk` back on every
//! Tab; the candidates come from the config and definitions, so they follow the bench. Nothing here
//! opens a device or a provider connection.

use std::ffi::OsStr;
use std::path::PathBuf;

use clap_complete::engine::CompletionCandidate;
use clap_complete::env::Shells;

use super::device::Names;
use super::skill::SKILLS;
use crate::config::{Config, Device};
use crate::declared::Type;
use crate::error::{DeskError, Result};
use crate::library::Library;
use crate::ops::{Catalog, split_channel};
use crate::output::Report;

/// The environment variable the shell script sets when it calls back.
pub const VAR: &str = "DESK_COMPLETE";

/// `desk completions <shell>`.
pub fn script(shell: &str) -> Result<Report> {
    let shells = Shells::builtins();
    let completer = shells
        .completer(shell)
        .ok_or_else(|| DeskError::Usage(format!("no completions for shell `{shell}`")))?;
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "desk".to_string());
    let mut buf = Vec::new();
    completer.write_registration(VAR, "desk", "desk", &exe, &mut buf)?;
    let mut script = String::from_utf8_lossy(&buf).into_owned();
    if shell == "bash" {
        script = with_bash_separators(&script);
    }
    Ok(Report::new(
        &serde_json::json!({ "shell": shell, "script": script }),
        script,
    ))
}

/// With the cursor right after a `:` or `=`, bash passes an empty word in its place, so `psu:` would
/// look like `psu `. Keep the separator as a word of its own and complete an empty one after it,
/// which [`Line`] glues back together. And with only options left (a number's value, say), don't
/// let readline fill in their common `--` where a value goes.
fn with_bash_separators(script: &str) -> String {
    let registered = script.replace("-F _clap_complete_desk", "-F _desk_complete");
    format!(
        r#"_desk_complete() {{
    if [[ "${{COMP_WORDS[COMP_CWORD]}}" == [:=] ]]; then
        COMP_WORDS+=("")
        COMP_CWORD=$((COMP_CWORD + 1))
    fi
    _clap_complete_desk "$@"
    if [[ "$2" != -* ]]; then
        local candidate
        for candidate in "${{COMPREPLY[@]}}"; do
            [[ "$candidate" == -* ]] || return 0
        done
        COMPREPLY=()
    fi
}}
{registered}"#
    )
}

/// A candidate and its description.
type Candidate = (String, Option<String>);

/// The completer of every positional argument: what's next depends on the whole command line.
pub fn candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let line = Line::parse(&words_before_cursor());
    let typed = format!("{}{}", line.glued, current.to_string_lossy());
    let config = Config::load(line.config.as_deref()).ok();
    let words: Vec<&str> = line.words.iter().map(String::as_str).collect();
    next(config.as_ref(), &words, &typed)
        .into_iter()
        .filter(|(value, _)| value.starts_with(&typed))
        .map(|(value, help)| {
            // Bash already has the part before a `:` or `=`: only the rest is replaced.
            let value = value[line.glued.len()..].to_string();
            CompletionCandidate::new(value).help(help.map(Into::into))
        })
        .collect()
}

/// The words of the command line up to the cursor: `DESK_COMPLETE=bash desk -- desk set psu vo`
/// with an index (bash, zsh, elvish), or ending at the cursor (fish, PowerShell).
fn words_before_cursor() -> Vec<String> {
    let mut words: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .skip_while(|a| a != "--")
        .skip(2)
        .collect();
    let cursor = std::env::var("_CLAP_COMPLETE_INDEX")
        .ok()
        .and_then(|i| i.parse::<usize>().ok())
        .map_or(words.len().saturating_sub(1), |i| i.saturating_sub(1));
    words.truncate(cursor);
    words
}

/// The command line, without options.
#[derive(Debug, Default, PartialEq)]
struct Line {
    words: Vec<String>,
    config: Option<PathBuf>,
    /// What belongs to the word at the cursor: bash splits words at `:` and `=`.
    glued: String,
}

impl Line {
    fn parse(words: &[String]) -> Line {
        let mut line = Line {
            config: std::env::var_os("DESK_CONFIG").map(PathBuf::from),
            ..Line::default()
        };
        let mut iter = words.iter().peekable();
        let mut only_positionals = false;
        let mut glue = false;
        while let Some(w) = iter.next() {
            if !only_positionals && w == "--" {
                only_positionals = true;
            } else if !only_positionals && w.starts_with('-') && w.len() > 1 {
                if let Some(path) = w.strip_prefix("--config=") {
                    line.config = Some(PathBuf::from(path));
                } else if matches!(
                    w.as_str(),
                    "--config" | "--interval" | "--count" | "--search" | "-s"
                ) {
                    if iter.peek().is_some_and(|n| *n == "=") {
                        iter.next();
                    }
                    let value = iter.next();
                    if let (true, Some(v)) = (w == "--config", value) {
                        line.config = Some(PathBuf::from(v));
                    }
                }
            } else if (w == ":" || w == "=") && !line.words.is_empty() {
                if let Some(last) = line.words.last_mut() {
                    last.push_str(w);
                }
                glue = true;
            } else if glue {
                if let Some(last) = line.words.last_mut() {
                    last.push_str(w);
                }
                glue = false;
            } else {
                line.words.push(w.clone());
            }
        }
        if glue {
            line.glued = line.words.pop().unwrap_or_default();
        }
        line
    }
}

/// What can come next after `words`, given what's typed so far.
fn next(config: Option<&Config>, words: &[&str], typed: &str) -> Vec<Candidate> {
    let device = |target: &str| {
        let d = config?.resolve_device(split_channel(target).0).ok()?;
        let cat = Catalog::of(d).ok()?;
        Some((d, cat))
    };
    let unused = |names: Vec<Candidate>, given: &[&str]| -> Vec<Candidate> {
        names
            .into_iter()
            .filter(|(n, _)| !given.contains(&n.as_str()))
            .collect()
    };
    match words {
        ["get" | "set" | "do" | "watch"] | ["device", "show"] => devices(config, typed),
        ["power", "on" | "off" | "toggle" | "status", given @ ..] => {
            unused(devices(config, typed), given)
        }
        ["get" | "watch", target, given @ ..] => device(target).map_or(Vec::new(), |(d, cat)| {
            let names = Names::of(d, &cat, None);
            let mut out = documented(&names.settings);
            out.extend(documented(&names.values));
            unused(out, given)
        }),
        ["set", target, rest @ ..] => device(target).map_or(Vec::new(), |(d, cat)| {
            if rest.len().is_multiple_of(2) {
                let given: Vec<&str> = rest.iter().step_by(2).copied().collect();
                unused(documented(&Names::of(d, &cat, None).settings), &given)
            } else {
                rest.last().map_or(Vec::new(), |s| setting_values(&cat, s))
            }
        }),
        ["do", target] => device(target).map_or(Vec::new(), |(d, cat)| {
            Names::of(d, &cat, None)
                .actions
                .iter()
                .map(|(n, a)| (n.clone(), a.action.doc.clone()))
                .collect()
        }),
        ["do", target, action, given @ ..] => device(target).map_or(Vec::new(), |(_, cat)| {
            action_args(&cat, action, given, typed)
        }),
        ["provider", "test" | "endpoints"] => config.map_or(Vec::new(), |c| {
            c.providers.keys().map(|p| (p.clone(), None)).collect()
        }),
        ["library", "show"] => definitions(config),
        ["library", "check", given @ ..] => unused(definitions(config), given),
        ["skill"] => SKILLS.iter().map(|(n, _)| (n.to_string(), None)).collect(),
        _ => Vec::new(),
    }
}

/// Device ids and aliases (one word only); once a `device:` is typed, its channels.
fn devices(config: Option<&Config>, typed: &str) -> Vec<Candidate> {
    let Some(config) = config else {
        return Vec::new();
    };
    if let Some((query, _)) = typed.split_once(':') {
        let Ok(d) = config.resolve_device(query) else {
            return Vec::new();
        };
        return channels_of(d)
            .into_iter()
            .map(|ch| (format!("{query}:{ch}"), Some(d.name.clone())))
            .collect();
    }
    let mut out = Vec::new();
    for d in &config.devices {
        out.push((d.id.clone(), Some(d.name.clone())));
        out.extend(
            d.aliases
                .iter()
                .filter(|a| !a.contains(char::is_whitespace))
                .map(|a| (a.clone(), Some(d.name.clone()))),
        );
    }
    out
}

fn channels_of(d: &Device) -> Vec<String> {
    let Ok(cat) = Catalog::of(d) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for ch in cat
        .entries
        .iter()
        .flat_map(|e| e.decls.channels.iter().flatten())
    {
        if !out.contains(ch) {
            out.push(ch.clone());
        }
    }
    out
}

fn documented(names: &indexmap::IndexMap<String, super::device::NameView>) -> Vec<Candidate> {
    names
        .iter()
        .map(|(n, v)| (n.clone(), v.decl.doc.clone()))
        .collect()
}

fn setting_values(cat: &Catalog, setting: &str) -> Vec<Candidate> {
    let Ok((_, _, decl)) = cat.setting(setting) else {
        return Vec::new();
    };
    choices_of(decl)
}

fn choices_of(decl: &crate::declared::Decl) -> Vec<Candidate> {
    match decl.kind {
        Type::Choice => decl.choices.iter().map(|c| (c.clone(), None)).collect(),
        Type::Boolean => vec![("on".to_string(), None), ("off".to_string(), None)],
        Type::Number | Type::String => Vec::new(),
    }
}

/// `seconds=`, and once an argument's name is there, its values (`mode=fast`).
fn action_args(cat: &Catalog, action: &str, given: &[&str], typed: &str) -> Vec<Candidate> {
    let Ok((_, _, decl)) = cat.action(action) else {
        return Vec::new();
    };
    let values = |name: &str| -> Vec<Candidate> {
        decl.args.get(name).map_or(Vec::new(), |d| {
            choices_of(d)
                .into_iter()
                .map(|(c, h)| (format!("{name}={c}"), h))
                .collect()
        })
    };
    if let Some((name, _)) = typed.split_once('=') {
        return values(name);
    }
    decl.args
        .iter()
        .filter(|(k, _)| {
            !given
                .iter()
                .any(|g| g.split_once('=').is_some_and(|(n, _)| n == *k))
        })
        .map(|(k, d)| (format!("{k}="), d.doc.clone()))
        .collect()
}

fn definitions(config: Option<&Config>) -> Vec<Candidate> {
    let Some(config) = config else {
        return Vec::new();
    };
    Library::for_config(&config.path)
        .list()
        .into_iter()
        .filter(|l| !l.shadowed)
        .map(|l| (l.id, None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Line {
        let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        Line::parse(&words)
    }

    /// The wrapper relies on the name of the generated function.
    #[test]
    fn the_bash_script_goes_through_the_wrapper() {
        let data = script("bash").ok().map(|r| r.data).unwrap_or_default();
        let text = data["script"].as_str().unwrap_or_default();
        assert!(text.contains("_clap_complete_desk() {"), "{text}");
        assert!(text.contains("-F _desk_complete desk"), "{text}");
        assert!(!text.contains("-F _clap_complete_desk"), "{text}");
    }

    #[test]
    fn options_are_not_positionals() {
        let line = parse(&[
            "--json", "--config", "x.toml", "watch", "--count", "3", "psu",
        ]);
        assert_eq!(line.words, ["watch", "psu"]);
        assert_eq!(line.config, Some(PathBuf::from("x.toml")));
        let line = parse(&["set", "psu", "--dry-run", "--", "offset", "-1v"]);
        assert_eq!(line.words, ["set", "psu", "offset", "-1v"]);
        let line = parse(&["--config=y.toml", "get"]);
        assert_eq!(line.config, Some(PathBuf::from("y.toml")));
    }

    #[test]
    fn bash_splits_words_at_colons_and_equals() {
        let line = parse(&["set", "psu", ":", "2", "voltage"]);
        assert_eq!(line.words, ["set", "psu:2", "voltage"]);
        assert_eq!(line.glued, "");
        let line = parse(&["set", "psu", ":"]);
        assert_eq!(line.words, ["set"]);
        assert_eq!(line.glued, "psu:");
        let line = parse(&["do", "iron", "boost", "seconds", "="]);
        assert_eq!(line.words, ["do", "iron", "boost"]);
        assert_eq!(line.glued, "seconds=");
    }
}
