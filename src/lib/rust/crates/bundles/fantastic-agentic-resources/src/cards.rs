//! Role cards: one file = a `---` YAML header (`name`, `purpose`, `tier`, `grants`)
//! and the directive text (the system prompt) after it.

use fantastic_kernel::yaml_io;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// The generic card that ships in the crate, listed even when no folder has a copy.
pub const PROJECT_LEAD: &str = include_str!("../cards/project_lead.md");

/// A card's grant key that stands for the registry's own id.
pub const REGISTRY_TOKEN: &str = "$registry";

/// One role.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    /// The role's name.
    pub name: String,
    /// One line: what the role is for.
    pub purpose: String,
    /// A model-tier KEY, looked up in the registry's `tiers` (never a model name).
    pub tier: String,
    /// The child's `can_call`; the key `$registry` stands for the registry.
    pub grants: Map<String, Value>,
    /// The directive text: the child's system prompt.
    pub directive: String,
}

impl Card {
    /// The card as the `role` verb answers it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "name": self.name, "purpose": self.purpose, "tier": self.tier,
            "grants": Value::Object(self.grants.clone()), "directive": self.directive,
        })
    }
}

fn field(h: &Map<String, Value>, key: &str, file: &str) -> Result<String, String> {
    match h.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(format!("card {file}: {key} required")),
    }
}

/// Parse one card. `file` names the card in refusals.
///
/// # Errors
/// The reason, as one line: `card <file>: missing header`, `card <file>: <field>
/// required`, `card <file>: grants must be a map of "*" or verb lists`.
pub fn parse_card(text: &str, file: &str) -> Result<Card, String> {
    let missing = || format!("card {file}: missing header");
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(missing)?;
    let (header, body) = match rest.find("\n---") {
        Some(i) => {
            let after = &rest[i + 4..];
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .or_else(|| after.is_empty().then_some(after))
                .ok_or_else(missing)?;
            (&rest[..i], body)
        }
        None => return Err(missing()),
    };
    let doc =
        yaml_io::load(header).map_err(|e| format!("card {file}: header is not YAML ({e})"))?;
    let Value::Object(h) = doc else {
        return Err(missing());
    };
    let name = field(&h, "name", file)?;
    let purpose = field(&h, "purpose", file)?;
    let tier = field(&h, "tier", file)?;
    let grants = match h.get("grants") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(g)) => {
            // the kernel's own shape check, with `$registry` standing for a real key
            let mut probe = Map::new();
            let swapped: Map<String, Value> = g
                .iter()
                .map(|(k, v)| {
                    let k = if k == REGISTRY_TOKEN {
                        "$self"
                    } else {
                        k.as_str()
                    };
                    (k.to_string(), v.clone())
                })
                .collect();
            probe.insert("can_call".to_string(), Value::Object(swapped));
            if fantastic_kernel::lifecycle::bad_grant_shape(&probe).is_some() {
                return Err(format!(
                    "card {file}: grants must be a map of \"*\" or verb lists"
                ));
            }
            g.clone()
        }
        Some(_) => {
            return Err(format!(
                "card {file}: grants must be a map of \"*\" or verb lists"
            ))
        }
    };
    Ok(Card {
        name,
        purpose,
        tier,
        grants,
        directive: body.trim().to_string(),
    })
}

/// Every card: the shipped `project_lead`, then each `*.md` of each folder in
/// `dirs` (files in name order). A bad card is listed in the errors and skipped;
/// a second card with a name already read from a folder is refused. A folder card
/// replaces the shipped one of the same name.
#[must_use]
pub fn load_dir(dirs: &[String]) -> (Vec<Card>, Vec<String>) {
    let mut cards: Vec<Card> = Vec::new();
    let mut errors = Vec::new();
    match parse_card(PROJECT_LEAD, "project_lead.md") {
        Ok(c) => cards.push(c),
        Err(e) => errors.push(e),
    }
    let mut from_dirs: HashSet<String> = HashSet::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            errors.push(format!("role dir {dir}: cannot be read"));
            continue;
        };
        let mut files: Vec<_> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort();
        for p in files {
            let file = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let text = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("card {file}: unreadable ({e})"));
                    continue;
                }
            };
            match parse_card(&text, &file) {
                Err(e) => errors.push(e),
                Ok(c) => {
                    if !from_dirs.insert(c.name.clone()) {
                        errors.push(format!("card {file}: duplicate role {}", c.name));
                        continue;
                    }
                    cards.retain(|k| k.name != c.name);
                    cards.push(c);
                }
            }
        }
    }
    (cards, errors)
}
