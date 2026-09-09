//! A small flag parser.
//!
//! Hand-rolled rather than derived, for one reason: it must REFUSE
//! single-dash long flags. Those are the Go tool's spelling, and silently
//! accepting both would let a copy-pasted Go instruction appear to work in a
//! repository where it measures the wrong thing.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Args {
    flags: BTreeMap<String, Option<String>>,
    /// Non-flag arguments. No command in this task reads these; kept for
    /// commands added later that take a positional argument.
    #[allow(dead_code)]
    pub positional: Vec<String>,
}

impl Args {
    /// `known` lists every long flag this command accepts, and whether it
    /// takes a value.
    pub fn parse(argv: &[String], known: &[(&str, bool)]) -> Result<Args, String> {
        let mut flags = BTreeMap::new();
        let mut positional = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            let arg = &argv[i];
            if arg == "-C" {
                let v = argv.get(i + 1).ok_or("-C needs a directory")?;
                flags.insert("C".to_string(), Some(v.clone()));
                i += 2;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (name, None),
                };
                let Some((_, takes_value)) = known.iter().find(|(k, _)| *k == name) else {
                    return Err(format!("unknown flag --{name}"));
                };
                if *takes_value {
                    let value = match inline {
                        Some(v) => v,
                        None => argv
                            .get(i + 1)
                            .ok_or(format!("--{name} needs a value"))?
                            .clone(),
                    };
                    let consumed = if arg.contains('=') { 1 } else { 2 };
                    flags.insert(name.to_string(), Some(value));
                    i += consumed;
                } else {
                    // `--force=false` must not be silently treated as
                    // `--force`: a reader who writes `=false` means "do not
                    // set this", and inserting it as present regardless of
                    // the value would do the opposite of what they asked.
                    if let Some(v) = inline {
                        return Err(format!("--{name} takes no value (got \"{v}\")"));
                    }
                    flags.insert(name.to_string(), None);
                    i += 1;
                }
                continue;
            }
            if arg.starts_with('-') && arg.len() > 1 {
                let name = arg.trim_start_matches('-');
                if known.iter().any(|(k, _)| *k == name) {
                    return Err(format!(
                        "unknown flag {arg} — this tool spells long flags with two dashes: \
                         --{name}"
                    ));
                }
                return Err(format!("unknown flag {arg}"));
            }
            positional.push(arg.clone());
            i += 1;
        }
        Ok(Args { flags, positional })
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags.get(name).and_then(|v| v.as_deref())
    }

    /// The `-C` directory, defaulting to the current one.
    pub fn dir(&self) -> PathBuf {
        PathBuf::from(self.value("C").unwrap_or("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const INIT_FLAGS: &[(&str, bool)] = &[("C", true), ("force", false)];

    #[test]
    fn dash_c_sets_the_directory() {
        let a = Args::parse(&argv(&["-C", "/tmp/repo"]), INIT_FLAGS).unwrap();
        assert_eq!(a.dir(), PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn a_no_value_flag_is_present_once_given() {
        let a = Args::parse(&argv(&["--force"]), INIT_FLAGS).unwrap();
        assert!(a.flag("force"));
    }

    #[test]
    fn an_unknown_long_flag_is_refused() {
        let err = Args::parse(&argv(&["--nope"]), INIT_FLAGS).unwrap_err();
        assert!(err.contains("--nope"), "{err}");
    }

    #[test]
    fn single_dash_long_flags_are_refused_and_name_the_two_dash_form() {
        let err = Args::parse(&argv(&["-force"]), INIT_FLAGS).unwrap_err();
        assert!(err.contains("--force"), "{err}");
    }

    #[test]
    fn a_value_flag_accepts_the_inline_equals_form() {
        let a = Args::parse(&argv(&["--force=yes"]), &[("force", true)]).unwrap();
        assert_eq!(a.value("force"), Some("yes"));
    }

    // `--force=false` must be a parse error, not a silent no-op: a flag that
    // takes no value has no way to represent "false", so accepting the
    // inline form and dropping the value would make `--force=false`
    // indistinguishable from `--force`, doing the exact opposite of what
    // whoever typed it meant.
    #[test]
    fn an_inline_value_on_a_no_value_flag_is_refused_rather_than_ignored() {
        let err = Args::parse(&argv(&["--force=false"]), INIT_FLAGS).unwrap_err();
        assert!(err.contains("--force"), "{err}");
        assert!(err.contains("false"), "{err}");
    }

    #[test]
    fn non_flag_arguments_are_collected_as_positional() {
        let a = Args::parse(&argv(&["foo", "bar"]), INIT_FLAGS).unwrap();
        assert_eq!(a.positional, vec!["foo".to_string(), "bar".to_string()]);
    }
}
