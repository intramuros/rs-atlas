# rustgraph3d

CLI that maps a Cargo project at file level (workspace + dependency crate sources) and
writes a single-page interactive 3D viewer. See README.md for user-facing docs.

## Layout

- `src/main.rs`: CLI (clap), `cargo metadata` (filtered to the host triple), BFS over the
  resolve graph to pick dependency crates by depth, builds one `Unit` per lib/bin target,
  `--prune`, JSON export, and `render()` which fills the viewer template.
- `src/model.rs`: data types (Package, Unit, ModInfo, UseDecl, Ref, FileInfo).
- `src/parse.rs`: walks each unit's module tree from its root file following `mod foo;`
  (`foo.rs`, `foo/mod.rs`, `#[path]`, inline mods). Records items, `use` declarations
  (all of them, with a `public` flag), `pub extern crate` re-exports, `#[macro_export]`
  macros, and qualified paths found by a `syn::visit::Visit` collector.
- `src/resolve.rs`: resolves paths to the defining file. `lookup` checks submodules,
  items, named uses, then glob uses (recursion capped by `MAX_DEPTH`); `resolve_path`
  handles `crate`/`self`/`super`/`::`, extern crates and 2015-edition crate-relative paths.
  `edges()` aggregates file->file edges with kinds `mod` / `use` / `path`.
- `src/serve.rs`: std-only localhost HTTP server: `/` (viewer, live mode), `/visit`
  (editor reports a file; with `from`/`fline`/`sym` it is a go-to-definition jump),
  `/events?since=N` (viewer polls; jumps carry a `jump` object), `/open?id=&line=` (runs
  `nvim --server ADDR --remote-send`, only for files in the graph). Routing is
  `State::route`, which the unit test calls directly.
- `nvim/atlas.lua`: the Neovim side (`:Atlas`, `:AtlasStop`, a `gd` that reports jumps,
  a BufEnter hook that reports visits). Copied or symlinked into the user's config.
- `src/viewer.html`: the viewer, embedded with `include_str!`. Placeholders replaced by
  `render()`: `__PROJECT__`, `/*__GRAPH__*/null`, `/*__LIVE__*/false`, `/*__NVIM__*/false`.
  Uses 3d-force-graph 1.79.1 from jsDelivr (global `ForceGraph3D`). Crate labels and the
  trail are an HTML/SVG overlay projected with `graph2ScreenCoords` each frame.
  The trail is a tree (`S.steps`, `S.cur`): jumps hang a labeled step under the step they
  started from, visits and clicks move along or extend the current branch; the bar shows
  one branch (`branch()`), the overlay draws them all.
  The file has no `<html>/<head>/<body>`; `render()` wraps it unless `--bare` is passed
  (bare output is for hosts that add their own document skeleton).

## Commands

```sh
cargo test                                   # resolver test on a synthetic two-crate tree
cargo run --release -- <project> -o out.html # generate a page
RG3D_DEBUG=1 cargo run --release -- <project> -o /dev/null   # list unresolved `use` paths
cargo run --release -- <project> --serve 7878                 # live mode
```

ripgrep (`git clone --depth 1 https://github.com/BurntSushi/ripgrep`) is a good test
project: ~424 files, ~1,660 links with default settings. Most unresolved paths there are
std/core/alloc, optional deps whose features are off, or crates beyond `--depth`.

## Known limits / ideas

- No type information: method calls (`x.foo()`) and trait-method dispatch are not edges.
- `#[cfg]` is ignored; all module files are included.
- Macro- or build.rs-generated modules and items are invisible.
- A file shared by two targets belongs to the first unit that loads it.
- The trail lives in the viewer only; a reload rebuilds it from the server's events, but
  steps added by clicking in the viewer are lost.
- Possible next steps: rust-analyzer (SCIP/LSIF) for precise edges, cfg evaluation via
  `cargo metadata` features, function-level nodes, trail export to the Neovim quickfix
  list, offline viewer (vendor the JS).
