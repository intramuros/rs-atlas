//! rustgraph3d — explore a Cargo project's files, and the dependency files
//! they reach into, as an interactive 3D graph.

mod model;
mod parse;
mod resolve;
mod serve;

use anyhow::{Context, Result, bail};
use cargo_metadata::{DependencyKind, MetadataCommand, TargetKind};
use clap::Parser;
use model::{Package, Unit};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(version, about = "Render a Cargo project's files and their cross-file dependencies as a 3D graph")]
struct Args {
    /// Project, workspace or worktree directory (or a Cargo.toml path)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// How many levels of dependency crates to include (0 = workspace only, 1 = direct deps)
    #[arg(short, long, default_value_t = 1)]
    depth: u32,

    /// Also follow dev-dependencies of workspace members
    #[arg(long)]
    dev: bool,

    /// Include bins, examples, tests and benches of workspace members (default: lib + bins)
    #[arg(long)]
    all_targets: bool,

    /// Dependency crates to leave out (repeatable), e.g. --exclude windows-sys
    #[arg(short = 'x', long)]
    exclude: Vec<String>,

    /// Only keep dependency files that the workspace reaches (directly or through other dep files)
    #[arg(long)]
    prune: bool,

    /// Output HTML file
    #[arg(short, long, default_value = "rustgraph3d.html")]
    output: PathBuf,

    /// Also write the raw graph as JSON
    #[arg(long)]
    json: Option<PathBuf>,

    /// Serve the viewer on this port and accept live "visit" events from your editor
    #[arg(long, value_name = "PORT")]
    serve: Option<u16>,

    /// Neovim server address (e.g. /tmp/nvim.sock) so clicking "Open" in the viewer opens the file in Neovim
    #[arg(long, value_name = "ADDR", requires = "serve")]
    nvim: Option<String>,

    /// Write the page without a document wrapper (for embedding in another host page)
    #[arg(long, hide = true)]
    bare: bool,
}

const VIEWER: &str = include_str!("viewer.html");

fn main() -> Result<()> {
    let args = Args::parse();
    let manifest = if args.path.is_dir() { args.path.join("Cargo.toml") } else { args.path.clone() };
    if !manifest.is_file() {
        bail!("no Cargo.toml at {}", manifest.display());
    }

    eprintln!("· reading cargo metadata …");
    let host = host_triple();
    let mut cmd = MetadataCommand::new();
    cmd.manifest_path(&manifest);
    if let Some(h) = &host {
        cmd.other_options(vec!["--filter-platform".into(), h.clone()]);
    }
    let meta = cmd.exec().context("cargo metadata failed")?;
    let resolve = meta.resolve.as_ref().context("cargo metadata returned no dependency graph")?;

    let pkg_by_id: HashMap<_, _> = meta.packages.iter().map(|p| (&p.id, p)).collect();
    let node_by_id: HashMap<_, _> = resolve.nodes.iter().map(|n| (&n.id, n)).collect();
    let ws: HashSet<_> = meta.workspace_members.iter().collect();

    // Breadth-first walk from the workspace members to assign depths.
    let mut depth_of = HashMap::new();
    let mut queue = VecDeque::new();
    for id in &meta.workspace_members {
        depth_of.insert(id, 0u32);
        queue.push_back(id);
    }
    while let Some(id) = queue.pop_front() {
        let d = depth_of[id];
        if d >= args.depth {
            continue;
        }
        let Some(node) = node_by_id.get(id) else { continue };
        for dep in &node.deps {
            let ok = dep.dep_kinds.iter().any(|k| match k.kind {
                DependencyKind::Normal => true,
                DependencyKind::Development => args.dev && d == 0,
                _ => false,
            });
            let name = pkg_by_id[&dep.pkg].name.as_str();
            if !ok || args.exclude.iter().any(|x| x == name) || depth_of.contains_key(&dep.pkg) {
                continue;
            }
            depth_of.insert(&dep.pkg, d + 1);
            queue.push_back(&dep.pkg);
        }
    }

    // Packages and their compilation units.
    let mut ids: Vec<_> = depth_of.keys().copied().collect();
    ids.sort_by_key(|id| (depth_of[id], pkg_by_id[id].name.to_string()));
    let mut packages = Vec::new();
    let mut units: Vec<Unit> = Vec::new();
    let mut lib_unit = HashMap::new();
    let mut pkg_units: Vec<(usize, &cargo_metadata::PackageId)> = Vec::new();
    for id in &ids {
        let p = pkg_by_id[id];
        let is_ws = ws.contains(id);
        let pi = packages.len();
        packages.push(Package {
            name: p.name.to_string(),
            version: p.version.to_string(),
            workspace: is_ws,
            depth: depth_of[id],
            dir: p.manifest_path.parent().map(|d| d.as_std_path().to_path_buf()).unwrap_or_default(),
            edition_2015: p.edition.as_str() == "2015",
        });
        for t in &p.targets {
            let is_lib = [TargetKind::Lib, TargetKind::RLib, TargetKind::ProcMacro, TargetKind::DyLib, TargetKind::CDyLib, TargetKind::StaticLib]
                .into_iter()
                .any(|k| t.is_kind(k));
            let wanted = is_lib
                || (is_ws && t.is_kind(TargetKind::Bin))
                || (is_ws && args.all_targets && [TargetKind::Example, TargetKind::Test, TargetKind::Bench].into_iter().any(|k| t.is_kind(k)));
            if !wanted || units.iter().any(|u| u.root == t.src_path.as_std_path()) {
                continue;
            }
            let ui = units.len();
            units.push(Unit {
                pkg: pi,
                target: t.name.clone(),
                extern_name: t.name.replace('-', "_"),
                root: t.src_path.as_std_path().to_path_buf(),
                deps: HashMap::new(),
            });
            if is_lib {
                lib_unit.insert(*id, ui);
            }
            pkg_units.push((ui, *id));
        }
    }
    // Wire up the extern names each unit can see.
    for &(ui, id) in &pkg_units {
        if let Some(node) = node_by_id.get(id) {
            for dep in &node.deps {
                if let Some(&du) = lib_unit.get(&dep.pkg) {
                    units[ui].deps.insert(dep.name.clone(), du);
                }
            }
        }
        // bins/tests see their own package's lib
        if let Some(&lu) = lib_unit.get(id) {
            if lu != ui {
                let name = units[lu].extern_name.clone();
                units[ui].deps.insert(name, lu);
            }
        }
    }

    eprintln!("· parsing {} crates ({} targets) …", packages.len(), units.len());
    let mut g = parse::Graph::new(packages, units);
    g.load_all();
    let edges = g.edges();

    let keep = if args.prune { reachable(&g, &edges) } else { vec![true; g.files.len()] };
    let out = to_json(&g, &edges, &keep, &meta.workspace_root.as_std_path().to_path_buf());

    let n_nodes = out["nodes"].as_array().map(|a| a.len()).unwrap_or(0);
    let n_links = out["links"].as_array().map(|a| a.len()).unwrap_or(0);
    eprintln!("· {n_nodes} files, {n_links} links ({} files failed to parse)", g.parse_errors.len());
    for (p, e) in g.parse_errors.iter().take(5) {
        eprintln!("  ! {}: {e}", p.display());
    }

    if let Some(j) = &args.json {
        std::fs::write(j, serde_json::to_string_pretty(&out)?)?;
        eprintln!("· wrote {}", j.display());
    }
    let html = render(&out, false, false, args.bare);
    std::fs::write(&args.output, &html)?;
    eprintln!("· wrote {}", args.output.display());

    if let Some(port) = args.serve {
        let live_html = render(&out, true, args.nvim.is_some(), false);
        let paths: HashMap<String, u32> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| (n["abs"].as_str().unwrap().to_string(), n["id"].as_u64().unwrap() as u32))
            .collect();
        serve::run(port, live_html, paths, args.nvim)?;
    }
    Ok(())
}

fn host_triple() -> Option<String> {
    let out = std::process::Command::new("rustc").arg("-vV").output().ok()?;
    String::from_utf8(out.stdout)
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
}

/// Files reachable from workspace files by following edges.
fn reachable(g: &parse::Graph, edges: &HashMap<(usize, usize), resolve::Edge>) -> Vec<bool> {
    let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(a, b) in edges.keys() {
        adj.entry(a).or_default().push(b);
    }
    let mut seen = vec![false; g.files.len()];
    let mut stack: Vec<usize> = (0..g.files.len())
        .filter(|&f| g.packages[g.units[g.files[f].unit].pkg].workspace)
        .collect();
    while let Some(f) = stack.pop() {
        if std::mem::replace(&mut seen[f], true) {
            continue;
        }
        if let Some(next) = adj.get(&f) {
            stack.extend(next.iter().copied().filter(|&n| !seen[n]));
        }
    }
    // Keep each kept crate's root file so the crate stays anchored.
    for u in &g.units {
        if let Ok(abs) = std::fs::canonicalize(&u.root) {
            if let Some(&f) = g.file_by_path.get(&abs) {
                if (0..g.files.len()).any(|o| seen[o] && g.files[o].unit == g.files[f].unit) {
                    seen[f] = true;
                }
            }
        }
    }
    seen
}

fn rel(p: &Path, base: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

fn to_json(g: &parse::Graph, edges: &HashMap<(usize, usize), resolve::Edge>, keep: &[bool], ws_root: &PathBuf) -> serde_json::Value {
    // Renumber kept files densely.
    let mut new_id = vec![u32::MAX; g.files.len()];
    let mut nodes = Vec::new();
    let mut files_per_pkg = vec![0usize; g.packages.len()];
    let mut loc_per_pkg = vec![0usize; g.packages.len()];
    for (fid, f) in g.files.iter().enumerate() {
        if !keep[fid] {
            continue;
        }
        let u = &g.units[f.unit];
        let p = &g.packages[u.pkg];
        new_id[fid] = nodes.len() as u32;
        files_per_pkg[u.pkg] += 1;
        loc_per_pkg[u.pkg] += f.loc;
        let modpath = std::iter::once(u.extern_name.as_str())
            .chain(f.modpath.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::");
        nodes.push(json!({
            "id": nodes.len(),
            "pkg": u.pkg,
            "target": u.target,
            "path": rel(&f.abs, &p.dir),
            "abs": f.abs.to_string_lossy(),
            "mod": modpath,
            "loc": f.loc,
            "items": f.items,
            "root": f.modpath.is_empty(),
        }));
    }
    let mut links: Vec<_> = edges
        .iter()
        .filter(|((a, b), _)| keep[*a] && keep[*b])
        .map(|((a, b), e)| {
            json!({
                "source": new_id[*a],
                "target": new_id[*b],
                "w": e.weight,
                "k": e.kinds.iter().collect::<Vec<_>>(),
                "n": e.names.iter().collect::<Vec<_>>(),
                "line": e.line,
            })
        })
        .collect();
    links.sort_by_key(|l| (l["source"].as_u64(), l["target"].as_u64()));
    let crates: Vec<_> = g
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            json!({
                "name": p.name,
                "version": p.version,
                "ws": p.workspace,
                "depth": p.depth,
                "dir": p.dir.to_string_lossy(),
                "files": files_per_pkg[i],
                "loc": loc_per_pkg[i],
            })
        })
        .collect();
    json!({
        "root": ws_root.to_string_lossy(),
        "name": ws_root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "crates": crates,
        "nodes": nodes,
        "links": links,
    })
}

fn render(data: &serde_json::Value, live: bool, nvim: bool, bare: bool) -> String {
    let payload = serde_json::to_string(data).unwrap().replace("</", "<\\/");
    let name = data["name"].as_str().unwrap_or("project");
    let name_html = name.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let page = VIEWER
        .replacen("__PROJECT__", &name_html, 1)
        .replace("/*__GRAPH__*/null", &payload)
        .replace("/*__LIVE__*/false", if live { "true" } else { "false" })
        .replace("/*__NVIM__*/false", if nvim { "true" } else { "false" });
    if bare {
        return page;
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover\">\n\
         <style>body{{margin:0}}</style>\n</head>\n<body>\n{page}\n</body>\n</html>\n"
    )
}
