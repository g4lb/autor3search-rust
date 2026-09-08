//! A small flag parser.
//!
//! Hand-rolled rather than derived, for one reason: it must REFUSE
//! single-dash long flags. Those are the Go tool's spelling, and silently
//! accepting both would let a copy-pasted Go instruction appear to work in a
//! repository where it measures the wrong thing.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
