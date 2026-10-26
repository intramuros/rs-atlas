//! Tiny localhost server: serves the viewer and relays editor events (file visits, and
//! go-to-definition jumps with the symbol they landed on) so the graph follows your
//! jumps, plus optional "open in Neovim" requests.

use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::str::FromStr;

struct Visit {
    seq: u64,
    id: Option<u32>,
    line: u32,
    file: String,
    /// Set when the editor reports a go-to-definition jump rather than a plain visit.
    jump: Option<Jump>,
}

/// Where a go-to-definition started, and the name it landed on.
struct Jump {
    from: Option<u32>,
    line: u32,
    sym: String,
}

struct State {
    html: String,
    paths: HashMap<String, u32>,
    by_id: HashMap<u32, String>,
    nvim: Option<String>,
    visits: Vec<Visit>,
}

type Reply = (&'static str, &'static str, Vec<u8>);

pub fn run(port: u16, html: String, paths: HashMap<String, u32>, nvim: Option<String>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let mut st = State::new(html, paths, nvim);
    eprintln!("· viewer: http://127.0.0.1:{port}/   (Ctrl-C to stop)");
    eprintln!("· editor hook: GET http://127.0.0.1:{port}/visit?file=<abs path>&line=<n>");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(e) = handle(stream, &mut st) {
            eprintln!("  ! request failed: {e}");
        }
    }
    Ok(())
}

fn handle(mut s: TcpStream, st: &mut State) -> Result<()> {
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    // Drain headers.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let (status, ctype, body) = st.route(first.split_whitespace().nth(1).unwrap_or("/"))?;
    reply(&mut s, status, ctype, &body)
}

impl State {
    fn new(html: String, paths: HashMap<String, u32>, nvim: Option<String>) -> Self {
        let by_id = paths.iter().map(|(p, &i)| (i, p.clone())).collect();
        State { html, paths, by_id, nvim, visits: Vec::new() }
    }

    /// Canonical path and graph id of a file the editor names.
    fn find(&self, file: &str) -> (String, Option<u32>) {
        let canon = std::fs::canonicalize(Path::new(file))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(file.to_string());
        let id = self.paths.get(&canon).copied();
        (canon, id)
    }

    fn route(&mut self, target: &str) -> Result<Reply> {
        let (route, query) = target.split_once('?').unwrap_or((target, ""));
        let q: HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (decode(k), decode(v)))
            .collect();

        Ok(match route {
            "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", self.html.clone().into_bytes()),
            "/visit" => {
                let (file, id) = self.find(q.get("file").map_or("", String::as_str));
                // `from` (with `fline` and `sym`) marks a go-to-definition jump.
                let jump = q.get("from").map(|from| Jump {
                    from: self.find(from).1,
                    line: num(&q, "fline").unwrap_or(1),
                    sym: q.get("sym").cloned().unwrap_or_default(),
                });
                // Collapse repeated visits to the same file; every jump counts.
                if jump.is_some() || id.is_none() || self.visits.last().map(|v| v.id) != Some(id) {
                    let seq = self.visits.len() as u64 + 1;
                    self.visits.push(Visit { seq, id, line: num(&q, "line").unwrap_or(1), file, jump });
                }
                let body = if id.is_some() { "ok\n" } else { "not in graph\n" };
                ("200 OK", "text/plain", body.into())
            }
            "/events" => {
                let since: u64 = num(&q, "since").unwrap_or(0);
                let items: Vec<_> = self
                    .visits
                    .iter()
                    .filter(|v| v.seq > since)
                    .map(|v| {
                        let mut e = serde_json::json!({"seq": v.seq, "id": v.id, "line": v.line, "file": v.file});
                        if let Some(j) = &v.jump {
                            e["jump"] = serde_json::json!({"from": j.from, "line": j.line, "sym": j.sym});
                        }
                        e
                    })
                    .collect();
                ("200 OK", "application/json", serde_json::to_vec(&items)?)
            }
            "/open" => {
                let path = num(&q, "id").and_then(|i: u32| self.by_id.get(&i));
                let (Some(addr), Some(path)) = (self.nvim.as_deref(), path) else {
                    return Ok(("404 Not Found", "text/plain", b"no neovim server or unknown file\n".to_vec()));
                };
                let line: u32 = num(&q, "line").unwrap_or(1);
                let escaped: String = path
                    .chars()
                    .flat_map(|c| if " \\%#|\"".contains(c) { vec!['\\', c] } else { vec![c] })
                    .collect();
                let keys = format!("<C-\\><C-N>:edit +{line} {escaped}<CR>");
                let status = std::process::Command::new("nvim").args(["--server", addr, "--remote-send", &keys]).status();
                match status {
                    Ok(st) if st.success() => ("200 OK", "text/plain", b"opened\n".to_vec()),
                    _ => ("502 Bad Gateway", "text/plain", b"could not reach neovim\n".to_vec()),
                }
            }
            _ => ("404 Not Found", "text/plain", b"not found\n".to_vec()),
        })
    }
}

fn num<T: FromStr>(q: &HashMap<String, String>, key: &str) -> Option<T> {
    q.get(key).and_then(|v| v.parse().ok())
}

fn reply(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> Result<()> {
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)?;
    Ok(())
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::State;
    use std::collections::HashMap;

    #[test]
    fn jumps_keep_origin_and_symbol() {
        let tmp = std::env::temp_dir().join(format!("rg3d-serve-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (a, b) = (tmp.join("a.rs"), tmp.join("b.rs"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap().to_string_lossy().to_string();
        let mut st = State::new(String::new(), HashMap::from([(canon(&a), 0), (canon(&b), 1)]), None);
        let (a, b) = (a.display(), b.display());

        st.route(&format!("/visit?file={a}&line=3")).unwrap();
        st.route(&format!("/visit?file={a}&line=5")).unwrap(); // same file again: collapsed
        st.route(&format!("/visit?file={b}&line=7&from={a}&fline=5&sym=Searcher")).unwrap();
        st.route(&format!("/visit?file={b}&line=9&from={b}&fline=7&sym=Sink")).unwrap(); // same-file jump: kept
        let (_, _, body) = st.route("/events?since=0").unwrap();
        let evs: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(evs.as_array().unwrap().len(), 3);
        assert!(evs[0]["jump"].is_null(), "plain visit");
        assert_eq!(evs[1]["id"], 1);
        assert_eq!(evs[1]["jump"], serde_json::json!({"from": 0, "line": 5, "sym": "Searcher"}));
        assert_eq!(evs[2]["jump"]["from"], 1);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
