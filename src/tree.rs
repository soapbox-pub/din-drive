//! Assembling the folder tree from nodes, following the spec's resolution rules:
//! parents come only from `y`, unresolvable parents and cycles go to the root,
//! trashed folders hide their contents, and duplicate names get a suffix.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::keys::{DriveKeys, ROOT};
use crate::model::NodeContent;

#[derive(Debug, Clone)]
pub struct Entry {
    pub y: String,
    pub content: NodeContent,
}

#[derive(Default)]
struct View {
    /// Visible children of each visible folder: display name -> id, sorted.
    children: HashMap<String, Vec<(String, String)>>,
    parent: HashMap<String, String>,
    name: HashMap<String, String>,
    /// Trashed nodes whose folder is visible (the top level of the trash).
    trash: Vec<String>,
}

pub struct Tree {
    keys: Arc<DriveKeys>,
    root_tag: String,
    nodes: HashMap<String, Entry>,
    view: Option<View>,
}

fn disambiguate(name: &str, id: &str) -> String {
    let short = &id[..id.len().min(6)];
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({short}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({short})"),
    }
}

impl Tree {
    pub fn new(keys: Arc<DriveKeys>) -> Self {
        let root_tag = keys.parent_tag(ROOT);
        Self {
            keys,
            root_tag,
            nodes: HashMap::new(),
            view: None,
        }
    }

    pub fn upsert(&mut self, d: &str, y: &str, content: NodeContent) {
        self.nodes.insert(
            d.to_string(),
            Entry {
                y: y.to_string(),
                content,
            },
        );
        self.view = None;
    }

    pub fn remove(&mut self, d: &str) {
        self.nodes.remove(d);
        self.view = None;
    }

    pub fn get(&self, d: &str) -> Option<&Entry> {
        self.nodes.get(d)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn view(&mut self) -> &View {
        if self.view.is_none() {
            self.view = Some(self.build());
        }
        self.view.as_ref().unwrap()
    }

    fn build(&self) -> View {
        // Folders that can have children: live (possibly trashed) dirs.
        let mut dir_by_tag: HashMap<String, &str> = HashMap::new();
        for (d, e) in &self.nodes {
            if e.content.is_dir() {
                dir_by_tag.insert(self.keys.parent_tag(d), d);
            }
        }
        // Raw parent of every live node.
        let mut raw: HashMap<&str, &str> = HashMap::new();
        for (d, e) in &self.nodes {
            if e.content.is_gone() {
                continue;
            }
            let p = if e.y == self.root_tag {
                ROOT
            } else {
                match dir_by_tag.get(&e.y) {
                    Some(p) if *p != d.as_str() => p,
                    _ => ROOT,
                }
            };
            raw.insert(d, p);
        }
        // Break cycles: any node whose ancestors loop goes to the root.
        let mut parent: HashMap<String, String> = HashMap::new();
        let mut settled: HashSet<&str> = HashSet::new();
        for &start in raw.keys() {
            if settled.contains(start) {
                continue;
            }
            let mut path: Vec<&str> = Vec::new();
            let mut on_path: HashSet<&str> = HashSet::new();
            let mut cur = start;
            loop {
                if cur == ROOT || settled.contains(cur) {
                    break;
                }
                if on_path.contains(cur) {
                    // Cycle: everything from `cur` onwards on the path loops.
                    let i = path.iter().position(|&n| n == cur).unwrap();
                    for &n in &path[i..] {
                        parent.insert(n.to_string(), ROOT.to_string());
                        settled.insert(n);
                    }
                    path.truncate(i);
                    break;
                }
                on_path.insert(cur);
                path.push(cur);
                cur = raw[cur];
            }
            for &n in &path {
                parent.entry(n.to_string()).or_insert_with(|| raw[n].to_string());
                settled.insert(n);
            }
        }
        // Visibility: not trashed, and in a visible folder.
        let mut visible: HashMap<&str, bool> = HashMap::new();
        fn is_visible<'a>(
            d: &'a str,
            nodes: &'a HashMap<String, Entry>,
            parent: &'a HashMap<String, String>,
            memo: &mut HashMap<&'a str, bool>,
        ) -> bool {
            if d == ROOT {
                return true;
            }
            if let Some(&v) = memo.get(d) {
                return v;
            }
            let v = match (nodes.get(d), parent.get(d)) {
                (Some(e), Some(p)) => e.content.trashed.is_none() && is_visible(p, nodes, parent, memo),
                _ => false,
            };
            memo.insert(d, v);
            v
        }
        let mut view = View::default();
        let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
        for d in parent.keys() {
            let e = &self.nodes[d];
            let p = parent[d].as_str();
            if is_visible(d, &self.nodes, &parent, &mut visible) {
                groups.entry(p).or_default().push(d);
            } else if e.content.trashed.is_some() && is_visible(p, &self.nodes, &parent, &mut visible) {
                view.trash.push(d.clone());
            }
        }
        for (p, mut ids) in groups {
            ids.sort();
            let mut taken: HashSet<&str> = HashSet::new();
            let mut list = Vec::new();
            for d in ids {
                let name = self.nodes[d].content.name.as_deref().unwrap_or(d);
                let shown = if taken.insert(name) {
                    name.to_string()
                } else {
                    disambiguate(name, d)
                };
                view.name.insert(d.to_string(), shown.clone());
                list.push((shown, d.to_string()));
            }
            list.sort();
            view.children.insert(p.to_string(), list);
        }
        view.parent = parent;
        view.trash.sort();
        view
    }

    /// Visible children of a folder, sorted by name.
    pub fn children(&mut self, dir: &str) -> Vec<(String, String)> {
        self.view().children.get(dir).cloned().unwrap_or_default()
    }

    pub fn lookup(&mut self, dir: &str, name: &str) -> Option<String> {
        let list = self.view().children.get(dir)?;
        list.binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .map(|i| list[i].1.clone())
    }

    /// Where a node is shown, if it's visible.
    pub fn parent(&mut self, d: &str) -> Option<String> {
        let v = self.view();
        v.name.contains_key(d).then(|| v.parent[d].clone())
    }

    pub fn display_name(&mut self, d: &str) -> Option<String> {
        self.view().name.get(d).cloned()
    }

    pub fn is_visible(&mut self, d: &str) -> bool {
        d == ROOT || self.view().name.contains_key(d)
    }

    pub fn trash(&mut self) -> Vec<String> {
        self.view().trash.clone()
    }

    /// A node and everything beneath it, regardless of visibility.
    pub fn subtree(&mut self, d: &str) -> Vec<String> {
        let view = self.view();
        let mut by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
        for (child, p) in &view.parent {
            by_parent.entry(p.as_str()).or_default().push(child.as_str());
        }
        let mut out = vec![d.to_string()];
        let mut i = 0;
        while i < out.len() {
            if let Some(kids) = by_parent.get(out[i].as_str()) {
                out.extend(kids.iter().map(|s| s.to_string()));
            }
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Arc<DriveKeys>, Tree) {
        let keys = Arc::new(DriveKeys::new([3; 32]));
        let tree = Tree::new(keys.clone());
        (keys, tree)
    }

    #[test]
    fn nesting_and_lookup() {
        let (k, mut t) = setup();
        t.upsert("a", &k.parent_tag(ROOT), NodeContent::dir("Docs", 1));
        t.upsert("b", &k.parent_tag("a"), NodeContent::file("x.txt", 1));
        assert_eq!(t.lookup(ROOT, "Docs").as_deref(), Some("a"));
        assert_eq!(t.lookup("a", "x.txt").as_deref(), Some("b"));
        assert_eq!(t.parent("b").as_deref(), Some("a"));
    }

    #[test]
    fn orphans_and_cycles_go_to_root() {
        let (k, mut t) = setup();
        t.upsert("o", &k.parent_tag("missing"), NodeContent::file("orphan", 1));
        t.upsert("p", &k.parent_tag("q"), NodeContent::dir("P", 1));
        t.upsert("q", &k.parent_tag("p"), NodeContent::dir("Q", 1));
        t.upsert("c", &k.parent_tag("q"), NodeContent::file("child", 1));
        let names: Vec<String> = t.children(ROOT).into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"orphan".to_string()));
        assert!(names.contains(&"P".to_string()) && names.contains(&"Q".to_string()));
        assert_eq!(t.parent("c").as_deref(), Some("q"));
    }

    #[test]
    fn duplicate_names() {
        let (k, mut t) = setup();
        t.upsert("bbbbbbbb", &k.parent_tag(ROOT), NodeContent::file("r.pdf", 1));
        t.upsert("aaaaaaaa", &k.parent_tag(ROOT), NodeContent::file("r.pdf", 1));
        assert_eq!(t.lookup(ROOT, "r.pdf").as_deref(), Some("aaaaaaaa"));
        assert_eq!(t.lookup(ROOT, "r (bbbbbb).pdf").as_deref(), Some("bbbbbbbb"));
    }

    #[test]
    fn trash_hides_subtree_and_gone_orphans() {
        let (k, mut t) = setup();
        let mut dir = NodeContent::dir("D", 1);
        dir.trashed = Some(5);
        t.upsert("d", &k.parent_tag(ROOT), dir);
        t.upsert("f", &k.parent_tag("d"), NodeContent::file("f", 1));
        assert!(t.children(ROOT).is_empty());
        assert!(!t.is_visible("f"));
        assert_eq!(t.trash(), vec!["d".to_string()]);
        // A gone folder's leftover children surface at the root.
        t.upsert("d", &k.parent_tag(ROOT), NodeContent::gone(6));
        assert_eq!(t.lookup(ROOT, "f").as_deref(), Some("f"));
    }
}
