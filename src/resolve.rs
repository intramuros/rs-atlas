//! Resolves recorded paths to the file that defines what they point at,
//! following `pub use` re-exports and glob imports.

use crate::parse::Graph;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone)]
enum Tgt {
    Mod(usize, Vec<String>),
    Item(usize),
}

const MAX_DEPTH: u32 = 10;

#[derive(Default)]
pub struct Edge {
    pub weight: u32,
    pub kinds: BTreeSet<&'static str>,
    pub names: BTreeSet<String>,
    pub line: u32,
}

impl Graph {
    fn tgt_file(&self, t: &Tgt) -> Option<usize> {
        match t {
            Tgt::Mod(u, p) => self.mods.get(&(*u, p.clone())).map(|m| m.file),
            Tgt::Item(f) => Some(*f),
        }
    }

    /// Looks up `name` inside module `m` of unit `u`.
    fn lookup(&self, u: usize, m: &[String], name: &str, private_ok: bool, depth: u32) -> Option<Tgt> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut sub = m.to_vec();
        sub.push(name.to_string());
        if self.mods.contains_key(&(u, sub.clone())) {
            return Some(Tgt::Mod(u, sub));
        }
        let info = self.mods.get(&(u, m.to_vec()))?;
        if let Some(&(fid, _)) = info.items.get(name) {
            return Some(Tgt::Item(fid));
        }
        for d in info.uses.iter().filter(|d| (d.public || private_ok) && d.name.as_deref() == Some(name)) {
            if let Some(t) = self.resolve_path(u, &d.ctx, &d.path, d.leading_colon, depth + 1) {
                return Some(t);
            }
        }
        for d in info.uses.iter().filter(|d| (d.public || private_ok) && d.name.is_none()) {
            if let Some(Tgt::Mod(u2, p2)) = self.resolve_path(u, &d.ctx, &d.path, d.leading_colon, depth + 1) {
                if (u2, &p2) != (u, &m.to_vec()) {
                    if let Some(t) = self.lookup(u2, &p2, name, false, depth + 1) {
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    fn resolve_path(&self, u: usize, ctx: &[String], segs: &[String], leading_colon: bool, depth: u32) -> Option<Tgt> {
        if depth > MAX_DEPTH || segs.is_empty() {
            return None;
        }
        let unit = &self.units[u];
        let first = segs[0].as_str();
        let (start, rest): (Tgt, &[String]) = if leading_colon {
            (Tgt::Mod(*unit.deps.get(first)?, vec![]), &segs[1..])
        } else {
            match first {
                "crate" => (Tgt::Mod(u, vec![]), &segs[1..]),
                "self" => (Tgt::Mod(u, ctx.to_vec()), &segs[1..]),
                "super" => {
                    let mut base = ctx.to_vec();
                    let mut i = 0;
                    while i < segs.len() && segs[i] == "super" {
                        base.pop()?;
                        i += 1;
                    }
                    (Tgt::Mod(u, base), &segs[i..])
                }
                "std" | "core" | "alloc" | "Self" => return None,
                _ => {
                    if let Some(t) = self.lookup(u, ctx, first, true, depth) {
                        (t, &segs[1..])
                    } else if let Some(&dep) = unit.deps.get(first) {
                        (Tgt::Mod(dep, vec![]), &segs[1..])
                    } else if self.packages[unit.pkg].edition_2015 && !ctx.is_empty() {
                        // 2015 edition: `use foo::bar` is crate-relative.
                        (self.lookup(u, &[], first, true, depth)?, &segs[1..])
                    } else {
                        return None;
                    }
                }
            }
        };
        let mut cur = start;
        for s in rest {
            match &cur {
                Tgt::Mod(uu, p) => match self.lookup(*uu, p, s, false, depth + 1) {
                    Some(t) => cur = t,
                    None => break,
                },
                Tgt::Item(_) => break, // associated item, method, enum variant...
            }
        }
        Some(cur)
    }

    /// Builds file -> file edges from `mod` declarations, `use`s and paths.
    pub fn edges(&self) -> HashMap<(usize, usize), Edge> {
        let mut out: HashMap<(usize, usize), Edge> = HashMap::new();
        let debug = std::env::var_os("RG3D_DEBUG").is_some();
        for (fid, f) in self.files.iter().enumerate() {
            for &(child, line) in &f.children {
                let e = out.entry((fid, child)).or_default();
                e.weight += 1;
                e.kinds.insert("mod");
                if e.line == 0 {
                    e.line = line;
                }
            }
            for r in &f.refs {
                let Some(t) = self.resolve_path(f.unit, &r.ctx, &r.segs, r.leading_colon, 0) else {
                    if debug && r.is_use {
                        eprintln!("  ? {}:{} {}", f.abs.display(), r.line, r.segs.join("::"));
                    }
                    continue;
                };
                let Some(dst) = self.tgt_file(&t) else { continue };
                if dst == fid {
                    continue;
                }
                let e = out.entry((fid, dst)).or_default();
                e.weight += 1;
                e.kinds.insert(if r.is_use { "use" } else { "path" });
                if e.names.len() < 8 {
                    if let Some(last) = r.segs.last() {
                        if last != "self" && last != "super" && last != "crate" {
                            e.names.insert(last.clone());
                        }
                    }
                }
                if e.line == 0 || r.line < e.line {
                    e.line = r.line;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::model::*;
    use crate::parse::Graph;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn write(dir: &std::path::Path, rel: &str, src: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, src).unwrap();
    }

    #[test]
    fn resolves_reexports_and_mods() {
        let tmp = std::env::temp_dir().join(format!("rg3d-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // dependency crate "dep"
        write(&tmp, "dep/src/lib.rs", "mod inner;\npub use inner::Thing;\npub mod de;\n#[macro_export]\nmacro_rules! mk { () => {} }\n");
        write(&tmp, "dep/src/inner.rs", "pub struct Thing;\n");
        write(&tmp, "dep/src/de/mod.rs", "mod imp;\npub use self::imp::*;\n");
        write(&tmp, "dep/src/de/imp.rs", "pub trait Deser {}\n");
        // main crate
        write(&tmp, "app/src/lib.rs", "pub mod a;\nmod b;\nuse dep::Thing;\nfn f() { dep::de::Deser::x(); dep::mk!(); }\n");
        write(&tmp, "app/src/a.rs", "pub mod nested;\npub fn go() { super::b::run(); }\n");
        write(&tmp, "app/src/a/nested.rs", "use crate::a::go;\n");
        write(&tmp, "app/src/b.rs", "pub fn run() {}\n");

        let pkg = |name: &str| Package {
            name: name.into(),
            version: "0.1.0".into(),
            workspace: name == "app",
            depth: 0,
            dir: tmp.join(name),
            edition_2015: false,
        };
        let units = vec![
            Unit { pkg: 0, target: "app".into(), extern_name: "app".into(), root: tmp.join("app/src/lib.rs"), deps: HashMap::from([("dep".to_string(), 1)]) },
            Unit { pkg: 1, target: "dep".into(), extern_name: "dep".into(), root: tmp.join("dep/src/lib.rs"), deps: HashMap::new() },
        ];
        let mut g = Graph::new(vec![pkg("app"), pkg("dep")], units);
        g.load_all();
        let id = |rel: &str| g.file_by_path[&std::fs::canonicalize(tmp.join(rel)).unwrap()];
        let edges = g.edges();
        let has = |a: &str, b: &str| edges.contains_key(&(id(a), id(b)));

        assert!(has("app/src/lib.rs", "app/src/a.rs"), "mod edge");
        assert!(has("app/src/a/nested.rs", "app/src/a.rs"), "crate:: path");
        assert!(has("app/src/a.rs", "app/src/b.rs"), "super:: path");
        assert!(has("app/src/lib.rs", "dep/src/inner.rs"), "pub use re-export");
        assert!(has("app/src/lib.rs", "dep/src/de/imp.rs"), "glob re-export");
        assert!(has("app/src/lib.rs", "dep/src/lib.rs"), "macro_export at root");
        let _ = PathBuf::new();
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
