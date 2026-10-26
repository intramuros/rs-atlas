//! Tiny localhost server: serves the viewer and relays editor "visit" events
//! (and optional "open in Neovim" requests) so the graph follows your jumps.

use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

struct Visit {
    seq: u64,
    id: Option<u32>,
    line: u32,
    file: String,
}

pub fn run(port: u16, html: String, paths: HashMap<String, u32>, nvim: Option<String>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let by_id: HashMap<u32, String> = paths.iter().map(|(p, &i)| (i, p.clone())).collect();
    eprintln!("· viewer: http://127.0.0.1:{port}/   (Ctrl-C to stop)");
    eprintln!("· editor hook: GET http://127.0.0.1:{port}/visit?file=<abs path>&line=<n>");
    let mut visits: Vec<Visit> = Vec::new();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(e) = handle(stream, &html, &paths, &by_id, &mut visits, nvim.as_deref()) {
            eprintln!("  ! request failed: {e}");
        }
    }
    Ok(())
}

fn handle(
    mut s: TcpStream,
    html: &str,
    paths: &HashMap<String, u32>,
    by_id: &HashMap<u32, String>,
    visits: &mut Vec<Visit>,
    nvim: Option<&str>,
) -> Result<()> {
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    // Drain headers.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let target = first.split_whitespace().nth(1).unwrap_or("/");
    let (route, query) = target.split_once('?').unwrap_or((target, ""));
    let q: HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect();

    match route {
        "/" | "/index.html" => reply(&mut s, "200 OK", "text/html; charset=utf-8", html.as_bytes()),
        "/visit" => {
            let file = q.get("file").cloned().unwrap_or_default();
            let canon = std::fs::canonicalize(Path::new(&file))
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(file.clone());
            let id = paths.get(&canon).copied();
            let line = q.get("line").and_then(|l| l.parse().ok()).unwrap_or(1);
            let seq = visits.len() as u64 + 1;
            // Collapse repeated events for the same file.
            if visits.last().map(|v| v.id) != Some(id) || id.is_none() {
                visits.push(Visit { seq, id, line, file: canon });
            }
            let body = if id.is_some() { "ok\n" } else { "not in graph\n" };
            reply(&mut s, "200 OK", "text/plain", body.as_bytes())
        }
        "/events" => {
            let since: u64 = q.get("since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let items: Vec<_> = visits
                .iter()
                .filter(|v| v.seq > since)
                .map(|v| serde_json::json!({"seq": v.seq, "id": v.id, "line": v.line, "file": v.file}))
                .collect();
            let body = serde_json::to_vec(&items)?;
            reply(&mut s, "200 OK", "application/json", &body)
        }
        "/open" => {
            let (Some(addr), Some(path)) = (nvim, q.get("id").and_then(|i| i.parse().ok()).and_then(|i: u32| by_id.get(&i))) else {
                return reply(&mut s, "404 Not Found", "text/plain", b"no neovim server or unknown file\n");
            };
            let line: u32 = q.get("line").and_then(|l| l.parse().ok()).unwrap_or(1);
            let escaped: String = path
                .chars()
                .flat_map(|c| if " \\%#|\"".contains(c) { vec!['\\', c] } else { vec![c] })
                .collect();
            let keys = format!("<C-\\><C-N>:edit +{line} {escaped}<CR>");
            let status = std::process::Command::new("nvim").args(["--server", addr, "--remote-send", &keys]).status();
            match status {
                Ok(st) if st.success() => reply(&mut s, "200 OK", "text/plain", b"opened\n"),
                _ => reply(&mut s, "502 Bad Gateway", "text/plain", b"could not reach neovim\n"),
            }
        }
        _ => reply(&mut s, "404 Not Found", "text/plain", b"not found\n"),
    }
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
