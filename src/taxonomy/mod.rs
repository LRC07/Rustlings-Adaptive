//! Concept taxonomy (v3 design §7.2, M3): hand-written `taxonomy/
//! concepts.toml` forms the fine-grained knowledge graph used as the
//! "fine track" anchor (vs. the coarse rustc error-code track).
//!
//! The graph provides:
//! - validation (unique ids, existing parents, acyclic),
//! - a reverse index from rustc error codes to concept ids,
//! - `link_templates`, which fills each node's `templates` list from the
//!   template library (single source of truth is `template.concepts`,
//!   so the TOML files never need to maintain the mapping by hand).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// One node in the concept graph. `templates` is filled programmatically
/// by [`ConceptGraph::link_templates`], not maintained in the TOML.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ConceptNode {
    /// Dotted id, e.g. `"borrow.split-fields"`.
    pub id: String,
    /// Chinese display name (CLI 文案用中文).
    pub name: String,
    /// Ids of broader concepts (graph edges upwards).
    #[serde(default)]
    pub parents: Vec<String>,
    /// rustc error codes typically triggered when this concept is missed.
    #[serde(default)]
    pub error_codes: Vec<String>,
    /// Known fix-pattern identifiers (used by later milestones).
    #[serde(default)]
    pub fix_patterns: Vec<String>,
    /// Template ids covering this concept (filled by `link_templates`).
    #[serde(default, skip_deserializing)]
    pub templates: Vec<String>,
}

/// The whole concept graph plus derived indexes.
#[derive(Debug, Clone, Default)]
pub struct ConceptGraph {
    nodes: BTreeMap<String, ConceptNode>,
}

/// Wire format of `taxonomy/concepts.toml`.
#[derive(Deserialize)]
struct ConceptsFile {
    #[serde(default, rename = "concept")]
    concepts: Vec<ConceptNode>,
}

/// DFS colors for cycle detection.
const WHITE: u8 = 0;
const GRAY: u8 = 1;
const BLACK: u8 = 2;

impl ConceptGraph {
    /// Load and validate `taxonomy/concepts.toml`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        Self::from_toml_str(&text).with_context(|| format!("解析 {} 失败", path.display()))
    }

    /// Parse and validate from a TOML string.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let file: ConceptsFile =
            toml::from_str(text).context("TOML 语法错误（[[concept]] 数组）")?;
        let mut nodes = BTreeMap::new();
        for node in file.concepts {
            if node.id.trim().is_empty() {
                bail!("存在缺少 id 的概念节点");
            }
            if node.name.trim().is_empty() {
                bail!("概念 '{}' 缺少 name", node.id);
            }
            if nodes.contains_key(&node.id) {
                bail!("概念 id 重复: '{}'", node.id);
            }
            nodes.insert(node.id.clone(), node);
        }
        let graph = Self { nodes };
        graph.validate()?;
        Ok(graph)
    }

    /// Structural validation: parents must exist and the graph must be
    /// acyclic along the `parents` edges.
    pub fn validate(&self) -> Result<()> {
        for node in self.nodes.values() {
            for p in &node.parents {
                if !self.nodes.contains_key(p) {
                    bail!("概念 '{}' 的父节点 '{}' 不存在", node.id, p);
                }
                if p == &node.id {
                    bail!("概念 '{}' 不能以自己为父节点", node.id);
                }
            }
        }
        // Cycle check along parent edges (three-color DFS).
        let mut color: BTreeMap<String, u8> =
            self.nodes.keys().map(|k| (k.clone(), WHITE)).collect();
        for id in self.nodes.keys() {
            if !self.cycle_dfs(id, &mut color) {
                bail!("概念图谱存在环（经过 '{}'）", id);
            }
        }
        Ok(())
    }

    fn cycle_dfs(&self, id: &str, color: &mut BTreeMap<String, u8>) -> bool {
        match color.get(id).copied().unwrap_or(BLACK) {
            GRAY => false, // back edge found
            _ => {
                color.insert(id.to_string(), GRAY);
                for p in &self.nodes[id].parents {
                    if !self.cycle_dfs(p, color) {
                        return false;
                    }
                }
                color.insert(id.to_string(), BLACK);
                true
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&ConceptNode> {
        self.nodes.get(id)
    }

    #[allow(dead_code)] // used by tests and later milestones (M6 profile)
    pub fn contains(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    #[allow(dead_code)] // used by tests and later milestones (M6 profile)
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[allow(dead_code)] // used by tests and later milestones (M6 profile)
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.nodes.keys()
    }

    /// All ids of concepts whose `parents` contain `id` (direct children).
    pub fn children_of(&self, id: &str) -> Vec<&str> {
        self.nodes
            .values()
            .filter(|n| n.parents.iter().any(|p| p == id))
            .map(|n| n.id.as_str())
            .collect()
    }

    /// Reverse index: which concepts are typically behind this error code.
    /// Returns sorted, deduplicated concept ids.
    pub fn concepts_for_code(&self, code: &str) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .nodes
            .values()
            .filter(|n| n.error_codes.iter().any(|c| c.eq_ignore_ascii_case(code)))
            .map(|n| n.id.as_str())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Every error code mentioned anywhere in the graph.
    #[allow(dead_code)] // used by the repo template fixture test / M6
    pub fn all_error_codes(&self) -> BTreeSet<&str> {
        self.nodes
            .values()
            .flat_map(|n| n.error_codes.iter().map(|s| s.as_str()))
            .collect()
    }

    /// Resolve a user-supplied query to a concept id: exact id match,
    /// then exact name match, then a unique substring match on id or
    /// name (case-insensitive). Returns `None` when nothing or several
    /// candidates match equally well.
    pub fn resolve(&self, query: &str) -> Option<&str> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        if let Some(n) = self.nodes.get(q) {
            return Some(&n.id);
        }
        if let Some(n) = self.nodes.values().find(|n| n.name == q) {
            return Some(&n.id);
        }
        let ql = q.to_lowercase();
        let hits: Vec<&str> = self
            .nodes
            .values()
            .filter(|n| {
                n.id.to_lowercase().contains(&ql) || n.name.contains(q)
            })
            .map(|n| n.id.as_str())
            .collect();
        if hits.len() == 1 {
            Some(hits[0])
        } else {
            None
        }
    }

    /// Fill each node's `templates` from the template library (each
    /// template lists the concepts it covers) and validate that every
    /// referenced concept exists. `items` is a list of
    /// `(template_id, concept_ids)`.
    pub fn link_templates<'a, I>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a [String])>,
    {
        let mut dangling: Vec<String> = Vec::new();
        for (tid, concepts) in items {
            for c in concepts {
                match self.nodes.get_mut(c.as_str()) {
                    Some(node) => {
                        if !node.templates.iter().any(|t| t == tid) {
                            node.templates.push(tid.to_string());
                        }
                    }
                    None => dangling.push(format!("{tid} → {c}")),
                }
            }
        }
        if dangling.is_empty() {
            Ok(())
        } else {
            bail!("模板引用了不存在的概念: {}", dangling.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[concept]]
id = "ownership"
name = "所有权"

[[concept]]
id = "ownership.move"
name = "所有权移动"
parents = ["ownership"]
error_codes = ["E0382", "E0505"]
fix_patterns = ["borrow-instead-of-move"]

[[concept]]
id = "ownership.copy"
name = "Copy 与 Clone"
parents = ["ownership"]
error_codes = ["E0382"]

[[concept]]
id = "borrow"
name = "借用"
"#;

    fn graph() -> ConceptGraph {
        ConceptGraph::from_toml_str(SAMPLE).unwrap()
    }

    #[test]
    fn parses_and_indexes() {
        let g = graph();
        assert_eq!(g.len(), 4);
        assert!(g.contains("ownership.move"));
        let n = g.get("ownership.move").unwrap();
        assert_eq!(n.name, "所有权移动");
        assert_eq!(n.parents, vec!["ownership"]);
        // `templates` is not part of the TOML wire format.
        assert!(n.templates.is_empty());
    }

    #[test]
    fn reverse_index_by_error_code() {
        let g = graph();
        assert_eq!(g.concepts_for_code("E0382"), vec!["ownership.copy", "ownership.move"]);
        // Case-insensitive; unknown codes yield nothing.
        assert_eq!(g.concepts_for_code("e0382"), vec!["ownership.copy", "ownership.move"]);
        assert!(g.concepts_for_code("E9999").is_empty());
        assert_eq!(g.all_error_codes().len(), 2);
    }

    #[test]
    fn children_and_validation() {
        let g = graph();
        assert_eq!(g.children_of("ownership"), vec!["ownership.copy", "ownership.move"]);
        assert_eq!(g.children_of("borrow"), Vec::<&str>::new());
    }

    #[test]
    fn rejects_missing_parent() {
        let bad = r#"
[[concept]]
id = "a"
name = "A"
parents = ["ghost"]
"#;
        let err = ConceptGraph::from_toml_str(bad).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn rejects_cycle_and_self_parent() {
        let cycle = r#"
[[concept]]
id = "a"
name = "A"
parents = ["b"]

[[concept]]
id = "b"
name = "B"
parents = ["a"]
"#;
        assert!(ConceptGraph::from_toml_str(cycle).is_err());

        let self_parent = r#"
[[concept]]
id = "a"
name = "A"
parents = ["a"]
"#;
        assert!(ConceptGraph::from_toml_str(self_parent).is_err());
    }

    #[test]
    fn rejects_duplicate_and_empty_ids() {
        let dup = r#"
[[concept]]
id = "a"
name = "A"

[[concept]]
id = "a"
name = "A2"
"#;
        assert!(ConceptGraph::from_toml_str(dup).is_err());
        assert!(ConceptGraph::from_toml_str("[[concept]]\nname = \"非空\"").is_err());
    }

    #[test]
    fn link_templates_fills_and_validates() {
        let mut g = graph();
        let concepts_own = ["ownership.move".to_string()];
        let concepts_bad = ["nope.here".to_string()];
        g.link_templates([("t-ok", &concepts_own[..])]).unwrap();
        assert_eq!(g.get("ownership.move").unwrap().templates, vec!["t-ok"]);

        let err = g.link_templates([("t-bad", &concepts_bad[..])]).unwrap_err();
        assert!(err.to_string().contains("t-bad → nope.here"), "{err}");
    }

    #[test]
    fn link_templates_is_idempotent() {
        let mut g = graph();
        let c = ["ownership.move".to_string()];
        g.link_templates([("t", &c[..])]).unwrap();
        g.link_templates([("t", &c[..])]).unwrap();
        assert_eq!(g.get("ownership.move").unwrap().templates.len(), 1);
    }

    #[test]
    fn resolve_query_variants() {
        let g = graph();
        assert_eq!(g.resolve("ownership.move"), Some("ownership.move"));
        assert_eq!(g.resolve("所有权移动"), Some("ownership.move"));
        assert_eq!(g.resolve("move"), Some("ownership.move"));
        // Exact id match wins even when substring search would be ambiguous.
        assert_eq!(g.resolve("ownership"), Some("ownership"));
        // Substring hits several nodes → ambiguous → None ("o" hits many).
        assert_eq!(g.resolve("o"), None);
        assert_eq!(g.resolve("没有的东西"), None);
        assert_eq!(g.resolve("  "), None);
    }

    #[test]
    fn parses_empty_file() {
        let g = ConceptGraph::from_toml_str("").unwrap();
        assert!(g.is_empty());
    }
}
