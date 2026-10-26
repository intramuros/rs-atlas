//! Walks a unit's module tree from its root file, following `mod foo;`
//! declarations, and records items, `use` declarations and path references.

use crate::model::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::spanned::Spanned;

pub struct Graph {
    pub packages: Vec<Package>,
    pub units: Vec<Unit>,
    pub files: Vec<FileInfo>,
    pub file_by_path: HashMap<PathBuf, usize>,
    pub mods: HashMap<(usize, Vec<String>), ModInfo>,
    pub parse_errors: Vec<(PathBuf, String)>,
}

/// Names that never refer to project or dependency modules.
const SKIP_FIRST: &[&str] = &["std", "core", "alloc", "Self", "proc_macro", "test"];

impl Graph {
    pub fn new(packages: Vec<Package>, units: Vec<Unit>) -> Self {
        Graph {
            packages,
            units,
            files: Vec::new(),
            file_by_path: HashMap::new(),
            mods: HashMap::new(),
            parse_errors: Vec::new(),
        }
    }

    pub fn load_all(&mut self) {
        for u in 0..self.units.len() {
            let root = self.units[u].root.clone();
            let dir = root.parent().map(Path::to_path_buf).unwrap_or_default();
            self.load_file(u, &root, Vec::new(), dir);
        }
    }

    /// Loads one file-backed module. Returns the file id, or `None` if missing.
    fn load_file(&mut self, unit: usize, path: &Path, modpath: Vec<String>, child_dir: PathBuf) -> Option<usize> {
        let abs = std::fs::canonicalize(path).ok()?;
        if let Some(&fid) = self.file_by_path.get(&abs) {
            // Already loaded (shared by two targets via #[path] or lib+bin). Map the module only.
            self.mods.entry((unit, modpath)).or_insert_with(|| ModInfo { file: fid, ..Default::default() });
            return Some(fid);
        }
        let src = std::fs::read_to_string(&abs).ok()?;
        let fid = self.files.len();
        self.files.push(FileInfo {
            unit,
            abs: abs.clone(),
            modpath: modpath.clone(),
            loc: src.lines().count(),
            items: Vec::new(),
            refs: Vec::new(),
            children: Vec::new(),
        });
        self.file_by_path.insert(abs.clone(), fid);
        self.mods.entry((unit, modpath.clone())).or_insert_with(|| ModInfo { file: fid, ..Default::default() });

        match syn::parse_file(&src) {
            Ok(file) => {
                let file_dir = abs.parent().map(Path::to_path_buf).unwrap_or_default();
                let mut cx = ItemCx { unit, fid, file_dir, top_level: true };
                self.process_items(&mut cx, &file.items, &modpath, &child_dir);
            }
            Err(e) => self.parse_errors.push((abs, e.to_string())),
        }
        Some(fid)
    }

    fn process_items(&mut self, cx: &mut ItemCx, items: &[syn::Item], modpath: &[String], child_dir: &Path) {
        let key = (cx.unit, modpath.to_vec());
        self.mods.entry(key.clone()).or_insert_with(|| ModInfo { file: cx.fid, ..Default::default() });

        for item in items {
            let line = item.span().start().line as u32;
            match item {
                syn::Item::Mod(m) => {
                    let name = m.ident.to_string();
                    let mut sub = modpath.to_vec();
                    sub.push(name.clone());
                    let path_attr = path_attr(&m.attrs);
                    self.define(cx, modpath, &name, "mod", line, is_pub(&m.vis));
                    if let Some((_, content)) = &m.content {
                        // Inline module: lives in this file; nested `mod x;` resolve under child_dir/name.
                        let dir = match &path_attr {
                            Some(p) => child_dir.join(p),
                            None => child_dir.join(&name),
                        };
                        let was_top = cx.top_level;
                        cx.top_level = false;
                        self.process_items(cx, content, &sub, &dir);
                        cx.top_level = was_top;
                    } else {
                        let candidates: Vec<PathBuf> = match &path_attr {
                            Some(p) if cx.top_level => vec![cx.file_dir.join(p)],
                            Some(p) => vec![child_dir.join(p)],
                            None => vec![child_dir.join(format!("{name}.rs")), child_dir.join(&name).join("mod.rs")],
                        };
                        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
                            let stem = found.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                            let parent = found.parent().map(Path::to_path_buf).unwrap_or_default();
                            let grand_dir = if stem == "mod" || path_attr.is_some() {
                                parent
                            } else {
                                parent.join(stem)
                            };
                            if let Some(child) = self.load_file(cx.unit, &found, sub, grand_dir) {
                                self.files[cx.fid].children.push((child, line));
                            }
                        }
                    }
                }
                syn::Item::Use(u) => {
                    let public = is_pub(&u.vis);
                    let mut flat = Vec::new();
                    flatten_use(&u.tree, &mut Vec::new(), &mut flat);
                    for (path, name) in flat {
                        if path.is_empty() {
                            continue;
                        }
                        self.files[cx.fid].refs.push(Ref {
                            ctx: modpath.to_vec(),
                            segs: path.clone(),
                            leading_colon: u.leading_colon.is_some(),
                            line,
                            is_use: true,
                        });
                        if let Some(n) = &name {
                            if public && cx.top_level {
                                self.summary(cx, "use", n, line, true);
                            }
                        }
                        self.mods.get_mut(&key).unwrap().uses.push(UseDecl {
                            name,
                            path,
                            leading_colon: u.leading_colon.is_some(),
                            ctx: modpath.to_vec(),
                            public,
                        });
                    }
                }
                syn::Item::Macro(m) => {
                    if let Some(ident) = &m.ident {
                        let exported = m.attrs.iter().any(|a| a.path().is_ident("macro_export"));
                        let name = format!("{ident}!");
                        self.define(cx, modpath, &name, "macro", line, exported);
                        if exported {
                            // #[macro_export] macros live at the crate root namespace.
                            let root = (cx.unit, Vec::new());
                            self.mods.entry(root).or_default().items.insert(ident.to_string(), (cx.fid, line));
                        }
                    }
                    self.collect_paths(cx.fid, modpath, item);
                }
                syn::Item::ExternCrate(e) => {
                    // `pub extern crate foo as bar;` re-exports a whole crate: treat it like `use ::foo as bar`.
                    let orig = e.ident.to_string();
                    let alias = e.rename.as_ref().map(|(_, i)| i.to_string()).unwrap_or_else(|| orig.clone());
                    if orig != "self" && orig != "std" && orig != "core" && orig != "alloc" {
                        self.files[cx.fid].refs.push(Ref {
                            ctx: modpath.to_vec(),
                            segs: vec![orig.clone()],
                            leading_colon: true,
                            line,
                            is_use: true,
                        });
                        self.mods.get_mut(&key).unwrap().uses.push(UseDecl {
                            name: Some(alias.clone()),
                            path: vec![orig],
                            leading_colon: true,
                            ctx: modpath.to_vec(),
                            public: is_pub(&e.vis),
                        });
                    }
                    if cx.top_level {
                        self.summary(cx, "extern crate", &alias, line, is_pub(&e.vis));
                    }
                }
                other => {
                    if let Some((kind, name, public)) = item_name(other) {
                        self.define(cx, modpath, &name, kind, line, public);
                    } else if let syn::Item::Impl(imp) = other {
                        if cx.top_level {
                            let ty = type_name(&imp.self_ty);
                            let label = match &imp.trait_ {
                                Some((_, p, _)) => format!("{} for {}", last_seg(p), ty),
                                None => ty,
                            };
                            self.summary(cx, "impl", &label, line, false);
                        }
                    }
                    self.collect_paths(cx.fid, modpath, other);
                }
            }
        }
    }

    fn define(&mut self, cx: &ItemCx, modpath: &[String], name: &str, kind: &'static str, line: u32, public: bool) {
        let key = (cx.unit, modpath.to_vec());
        let bare = name.trim_end_matches('!').to_string();
        self.mods.entry(key).or_default().items.insert(bare, (cx.fid, line));
        if cx.top_level {
            self.summary(cx, kind, name, line, public);
        }
    }

    fn summary(&mut self, cx: &ItemCx, kind: &'static str, name: &str, line: u32, public: bool) {
        let items = &mut self.files[cx.fid].items;
        if items.len() < 120 {
            items.push(ItemSummary { kind, name: name.to_string(), line, public });
        }
    }

    fn collect_paths(&mut self, fid: usize, modpath: &[String], item: &syn::Item) {
        let mut c = PathCollector { ctx: modpath.to_vec(), refs: Vec::new() };
        c.visit_item(item);
        self.files[fid].refs.extend(c.refs);
    }
}

struct ItemCx {
    unit: usize,
    fid: usize,
    file_dir: PathBuf,
    /// False while inside an inline `mod {}` block.
    top_level: bool,
}

struct PathCollector {
    ctx: Vec<String>,
    refs: Vec<Ref>,
}

impl<'ast> Visit<'ast> for PathCollector {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        let keep = segs.len() >= 2 && !SKIP_FIRST.contains(&segs[0].as_str());
        if keep {
            self.refs.push(Ref {
                ctx: self.ctx.clone(),
                segs,
                leading_colon: p.leading_colon.is_some(),
                line: p.span().start().line as u32,
                is_use: false,
            });
        }
        syn::visit::visit_path(self, p);
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        // `use` inside function bodies.
        let mut flat = Vec::new();
        flatten_use(&u.tree, &mut Vec::new(), &mut flat);
        let line = u.span().start().line as u32;
        for (path, _) in flat {
            if !path.is_empty() {
                self.refs.push(Ref {
                    ctx: self.ctx.clone(),
                    segs: path,
                    leading_colon: u.leading_colon.is_some(),
                    line,
                    is_use: true,
                });
            }
        }
    }

    // Skip nested inline modules inside fn bodies: rare, and their ctx would be wrong.
    fn visit_item_mod(&mut self, _m: &'ast syn::ItemMod) {}
}

/// Flattens a use tree into (path, imported name) pairs; name is `None` for globs.
fn flatten_use(tree: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, Option<String>)>) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            flatten_use(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let ident = n.ident.to_string();
            if ident == "self" {
                // `use foo::{self}` imports `foo` itself.
                let name = prefix.last().cloned();
                out.push((prefix.clone(), name));
            } else {
                let mut p = prefix.clone();
                p.push(ident.clone());
                out.push((p, Some(ident)));
            }
        }
        syn::UseTree::Rename(r) => {
            let mut p = prefix.clone();
            let ident = r.ident.to_string();
            if ident != "self" {
                p.push(ident);
            }
            let alias = r.rename.to_string();
            out.push((p, if alias == "_" { None } else { Some(alias) }));
        }
        syn::UseTree::Glob(_) => out.push((prefix.clone(), None)),
        syn::UseTree::Group(g) => {
            for t in &g.items {
                flatten_use(t, prefix, out);
            }
        }
    }
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for a in attrs {
        if a.path().is_ident("path") {
            if let syn::Meta::NameValue(nv) = &a.meta {
                if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = &nv.value {
                    return Some(s.value());
                }
            }
        }
    }
    None
}

fn is_pub(v: &syn::Visibility) -> bool {
    !matches!(v, syn::Visibility::Inherited)
}

fn item_name(item: &syn::Item) -> Option<(&'static str, String, bool)> {
    use syn::Item::*;
    Some(match item {
        Fn(f) => ("fn", f.sig.ident.to_string(), is_pub(&f.vis)),
        Struct(s) => ("struct", s.ident.to_string(), is_pub(&s.vis)),
        Enum(e) => ("enum", e.ident.to_string(), is_pub(&e.vis)),
        Trait(t) => ("trait", t.ident.to_string(), is_pub(&t.vis)),
        TraitAlias(t) => ("trait", t.ident.to_string(), is_pub(&t.vis)),
        Type(t) => ("type", t.ident.to_string(), is_pub(&t.vis)),
        Const(c) => ("const", c.ident.to_string(), is_pub(&c.vis)),
        Static(s) => ("static", s.ident.to_string(), is_pub(&s.vis)),
        Union(u) => ("union", u.ident.to_string(), is_pub(&u.vis)),
        ExternCrate(e) => (
            "extern crate",
            e.rename.as_ref().map(|(_, i)| i.to_string()).unwrap_or_else(|| e.ident.to_string()),
            is_pub(&e.vis),
        ),
        _ => return None,
    })
}

fn last_seg(p: &syn::Path) -> String {
    p.segments.last().map(|s| s.ident.to_string()).unwrap_or_default()
}

fn type_name(t: &syn::Type) -> String {
    match t {
        syn::Type::Path(tp) => last_seg(&tp.path),
        syn::Type::Reference(r) => format!("&{}", type_name(&r.elem)),
        other => {
            let s = quote_ish(other);
            if s.len() > 40 { format!("{}…", &s[..s.char_indices().nth(38).map(|(i, _)| i).unwrap_or(s.len())]) } else { s }
        }
    }
}

fn quote_ish(t: &syn::Type) -> String {
    quote::ToTokens::to_token_stream(t).to_string()
}
