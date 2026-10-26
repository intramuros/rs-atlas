//! Shared data model: compilation units, modules, files and references.

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// A Cargo package that takes part in the graph.
#[derive(Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub workspace: bool,
    /// 0 = workspace member, 1 = direct dependency, ...
    pub depth: u32,
    pub dir: PathBuf,
    pub edition_2015: bool,
}

/// One compilation unit (a lib, bin, or proc-macro target of a package).
#[derive(Debug)]
pub struct Unit {
    pub pkg: usize,
    pub target: String,
    /// The name other crates use to refer to this unit (`serde_json`, not `serde-json`).
    pub extern_name: String,
    pub root: PathBuf,
    /// extern name -> unit index of each dependency visible to this unit.
    pub deps: HashMap<String, usize>,
}

/// A `use` statement remembered for name resolution (both private and public).
#[derive(Debug, Clone)]
pub struct UseDecl {
    /// `None` for a glob import (`use foo::*`).
    pub name: Option<String>,
    pub path: Vec<String>,
    pub leading_colon: bool,
    /// Module the `use` appears in.
    pub ctx: Vec<String>,
    pub public: bool,
}

/// A module (file-backed or inline) inside a unit.
#[derive(Debug, Default)]
pub struct ModInfo {
    pub file: usize,
    /// item name -> (file, line)
    pub items: HashMap<String, (usize, u32)>,
    pub uses: Vec<UseDecl>,
}

/// A path that appears in source and may point at another file.
#[derive(Debug, Clone)]
pub struct Ref {
    pub ctx: Vec<String>,
    pub segs: Vec<String>,
    pub leading_colon: bool,
    pub line: u32,
    pub is_use: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemSummary {
    pub kind: &'static str,
    pub name: String,
    pub line: u32,
    #[serde(rename = "pub")]
    pub public: bool,
}

#[derive(Debug)]
pub struct FileInfo {
    pub unit: usize,
    pub abs: PathBuf,
    pub modpath: Vec<String>,
    pub loc: usize,
    pub items: Vec<ItemSummary>,
    pub refs: Vec<Ref>,
    /// Files declared with `mod foo;` from this file: (child file, line).
    pub children: Vec<(usize, u32)>,
}
