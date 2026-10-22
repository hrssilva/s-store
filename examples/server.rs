//! A minimal SPARQL endpoint over HTTP, standard library only.
//!
//! ```text
//! cargo run --release --example server -- [--bind 127.0.0.1:7878] [--workers N] [FILE[=GRAPH]]...
//! ```
//!
//! Every `FILE` (`.nt` or `.ttl`) is loaded before serving, into the default
//! graph or, with `=GRAPH`, into that named graph. N-Triples files are parsed
//! by `--workers` threads (default: the available cores); Turtle by one.
//! Then:
//!
//! - `GET /`: a page for running queries and uploading data.
//! - `GET /sparql?query=...` or `POST /sparql` (body `application/sparql-query`,
//!   or a form with `query=...`): SPARQL results. SELECT/ASK answer as
//!   `application/sparql-results+json`, or as TSV (`format=tsv` or
//!   `Accept: text/tab-separated-values`); CONSTRUCT/DESCRIBE as N-Triples.
//! - `POST /load?format=nt|ttl[&graph=IRI][&base=IRI]`: loads the request body.
//!   Into a non-empty graph, the new triples are edits of its FST, sealed
//!   into blocks beside it after each upload (milliseconds; see
//!   `Store::seal`).
//! - `POST /compact`: repacks every graph's FST, reclaiming the records that
//!   uploads made obsolete (`Store::shrink_to_fit`; proportional to the
//!   graph, so run it after a burst of uploads rather than after each).
//! - `GET /stats`: triple count and memory.
//!
//! Queries run concurrently under a read lock; a load takes the write lock.
//! Each connection is one request (`Connection: close`). The server has no
//! authentication: it binds to localhost unless told otherwise.

use s_parser::FileStream;
use s_store::load::{Format, LoadOptions, load, load_parallel, load_slice};
use s_store::sparql::QueryOutput;
use s_store::{GraphRef, Store};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Largest request body accepted (an upload is held in memory to load it).
const MAX_BODY: usize = 1 << 30;

fn main() {
    let mut bind = "127.0.0.1:7878".to_string();
    let mut workers = std::thread::available_parallelism().map_or(1, usize::from);
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().expect("--bind takes an address"),
            "--workers" => workers = args.next().and_then(|n| n.parse().ok()).expect("--workers takes a number"),
            "-h" | "--help" => {
                eprintln!("usage: server [--bind ADDR] [--workers N] [FILE[=GRAPH]]...");
                return;
            }
            _ => files.push(arg),
        }
    }

    let mut store = Store::new();
    for spec in &files {
        let (path, graph) = match spec.split_once('=') {
            Some((path, graph)) => (path, Some(angle(graph))),
            None => (spec.as_str(), None),
        };
        let format = Format::from_path(path.as_ref()).unwrap_or_else(|| panic!("{path}: expected a .nt or .ttl file"));
        let graph = graph.as_deref().map_or(GraphRef::Default, GraphRef::Named);
        let t = Instant::now();
        // Memory-mapped windows: the file is never read into memory whole.
        let stream = unsafe { FileStream::open(path, 4 << 20) }.unwrap_or_else(|e| panic!("{path}: {e}"));
        let options = LoadOptions { graph, ..Default::default() };
        let stats = match format {
            Format::NTriples => load_parallel(&mut store, || stream.clone(), &options, workers),
            Format::Turtle => load(&mut store, stream, format, &options),
        }
        .unwrap_or_else(|e| panic!("{path}: {e}"));
        eprintln!("loaded {path}: {} triples ({} new) in {:.2?}", stats.triples, stats.inserted, t.elapsed());
    }
    store.shrink_to_fit();
    eprintln!("{} triples, {} bytes", store.len(), store.size_bytes());

    let store = Arc::new(RwLock::new(store));
    let listener = TcpListener::bind(&bind).unwrap_or_else(|e| panic!("bind {bind}: {e}"));
    eprintln!("listening on http://{bind}/");
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            if let Err(e) = serve(conn, &store) {
                eprintln!("connection: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    params: Vec<(String, String)>,
    content_type: String,
    accept: String,
    body: Vec<u8>,
}

impl Request {
    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

fn reply(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
    Response { status, content_type, body: body.into() }
}

fn serve(conn: TcpStream, store: &RwLock<Store>) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let response = match read_request(&mut reader) {
        Ok(request) => route(&request, store),
        Err(e) => reply("400 Bad Request", "text/plain", e),
    };
    let mut out = conn;
    write!(out, "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n", response.status, response.content_type, response.body.len())?;
    out.write_all(&response.body)?;
    out.flush()
}

fn read_request(reader: &mut impl BufRead) -> Result<Request, String> {
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return Err("malformed request line".into()) };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request { method: method.to_string(), path: path.to_string(), params: parse_form(query), content_type: String::new(), accept: String::new(), body: Vec::new() };

    let mut length = 0usize;
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else { continue };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().map_err(|_| "bad Content-Length")?,
            "content-type" => request.content_type = value.to_ascii_lowercase(),
            "accept" => request.accept = value.to_ascii_lowercase(),
            "transfer-encoding" if !value.eq_ignore_ascii_case("identity") => return Err("chunked bodies are not supported; send Content-Length".into()),
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err(format!("body larger than {MAX_BODY} bytes"));
    }
    request.body = vec![0; length];
    reader.read_exact(&mut request.body).map_err(|e| e.to_string())?;
    if request.content_type.starts_with("application/x-www-form-urlencoded") {
        let form = String::from_utf8_lossy(&request.body).into_owned();
        request.params.extend(parse_form(&form));
    }
    Ok(request)
}

fn route(request: &Request, store: &RwLock<Store>) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => reply("200 OK", "text/html; charset=utf-8", PAGE),
        ("GET" | "POST", "/sparql") => sparql(request, store),
        ("POST", "/load") => upload(request, store),
        ("POST", "/compact") => {
            let mut store = store.write().expect("store lock");
            let t = Instant::now();
            store.shrink_to_fit();
            let elapsed = t.elapsed();
            eprintln!("compacted in {elapsed:.2?}");
            reply("200 OK", "application/json", format!("{{\"ms\":{:.3},\"bytes\":{}}}", elapsed.as_secs_f64() * 1e3, store.size_bytes()))
        }
        ("GET", "/stats") => {
            let store = store.read().expect("store lock");
            reply("200 OK", "application/json", format!("{{\"triples\":{},\"bytes\":{}}}", store.len(), store.size_bytes()))
        }
        (_, "/" | "/sparql" | "/load" | "/compact" | "/stats") => reply("405 Method Not Allowed", "text/plain", "method not allowed"),
        _ => reply("404 Not Found", "text/plain", "not found"),
    }
}

fn sparql(request: &Request, store: &RwLock<Store>) -> Response {
    let query = if request.method == "POST" && request.content_type.starts_with("application/sparql-query") {
        String::from_utf8_lossy(&request.body).into_owned()
    } else {
        match request.param("query") {
            Some(q) => q.to_string(),
            None => return reply("400 Bad Request", "text/plain", "missing `query`"),
        }
    };
    let tsv = request.param("format") == Some("tsv") || (request.param("format").is_none() && request.accept.contains("text/tab-separated-values"));
    let store = store.read().expect("store lock");
    let t = Instant::now();
    let output = match store.query(&query) {
        Ok(output) => output,
        Err(e) => return reply("400 Bad Request", "text/plain", e.to_string()),
    };
    let elapsed = t.elapsed();
    let response = match output {
        QueryOutput::Solutions(solutions) => {
            let mut out = String::new();
            let key = |id: u32| solutions.key(&store, id);
            if tsv {
                let header: Vec<String> = solutions.variables().iter().map(|v| format!("?{v}")).collect();
                out.push_str(&header.join("\t"));
                out.push('\n');
                for i in 0..solutions.len() {
                    let cells: Vec<String> = solutions.row(i).iter().map(|&id| key(id).unwrap_or_default()).collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
                reply("200 OK", "text/tab-separated-values; charset=utf-8", out)
            } else {
                let vars: Vec<String> = solutions.variables().iter().map(|v| json_string(v)).collect();
                out.push_str(&format!("{{\"head\":{{\"vars\":[{}]}},\"results\":{{\"bindings\":[", vars.join(",")));
                for i in 0..solutions.len() {
                    out.push_str(if i == 0 { "{" } else { ",{" });
                    let mut first = true;
                    for (name, &id) in solutions.variables().iter().zip(solutions.row(i)) {
                        let Some(k) = key(id) else { continue };
                        out.push_str(if first { "" } else { "," });
                        first = false;
                        out.push_str(&format!("{}:{}", json_string(name), json_term(&k)));
                    }
                    out.push('}');
                }
                out.push_str("]}}");
                reply("200 OK", "application/sparql-results+json", out)
            }
        }
        QueryOutput::Boolean(b) if tsv => reply("200 OK", "text/plain", b.to_string()),
        QueryOutput::Boolean(b) => reply("200 OK", "application/sparql-results+json", format!("{{\"head\":{{}},\"boolean\":{b}}}")),
        QueryOutput::Graph(triples) => {
            let mut out = String::new();
            for i in 0..triples.len() {
                let [s, p, o] = triples.triple(i).map(|id| triples.key(&store, id).unwrap_or_default());
                out.push_str(&format!("{s} {p} {o} .\n"));
            }
            reply("200 OK", "application/n-triples; charset=utf-8", out)
        }
    };
    eprintln!("query in {elapsed:.2?}: {}", query.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect::<String>());
    response
}

fn upload(request: &Request, store: &RwLock<Store>) -> Response {
    let format = match request.param("format").unwrap_or("nt") {
        "nt" | "ntriples" => Format::NTriples,
        "ttl" | "turtle" => Format::Turtle,
        other => return reply("400 Bad Request", "text/plain", format!("unknown format `{other}` (nt or ttl)")),
    };
    let graph_iri = request.param("graph").filter(|g| !g.is_empty()).map(angle);
    let graph = graph_iri.as_deref().map_or(GraphRef::Default, GraphRef::Named);
    let base = request.param("base").filter(|b| !b.is_empty());
    let mut store = store.write().expect("store lock");
    let t = Instant::now();
    let loaded = load_slice(&mut store, &request.body, format, &LoadOptions { graph, base, ..Default::default() });
    // Even a failed load may have inserted some triples.
    store.seal();
    match loaded {
        Ok(stats) => {
            let elapsed = t.elapsed();
            eprintln!("loaded {} triples ({} new) in {elapsed:.2?}", stats.triples, stats.inserted);
            reply("200 OK", "application/json", format!("{{\"triples\":{},\"inserted\":{},\"ms\":{:.3},\"total\":{}}}", stats.triples, stats.inserted, elapsed.as_secs_f64() * 1e3, store.len()))
        }
        Err(e) => reply("400 Bad Request", "text/plain", e.to_string()),
    }
}

/// A graph name as a key: `<iri>`, whether or not it came bracketed.
fn angle(iri: &str) -> String {
    if iri.starts_with('<') { iri.to_string() } else { format!("<{iri}>") }
}

/// `application/x-www-form-urlencoded` pairs.
fn parse_form(s: &str) -> Vec<(String, String)> {
    s.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (percent_decode(k), percent_decode(v))).collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A term key (N-Triples syntax) as a SPARQL JSON results term.
fn json_term(key: &str) -> String {
    if let Some(iri) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
        return format!("{{\"type\":\"uri\",\"value\":{}}}", json_string(&unescape(iri)));
    }
    if let Some(label) = key.strip_prefix("_:") {
        return format!("{{\"type\":\"bnode\",\"value\":{}}}", json_string(label));
    }
    let close = key.rfind('"').filter(|&i| i > 0 && key.starts_with('"'));
    let Some(close) = close else { return format!("{{\"type\":\"literal\",\"value\":{}}}", json_string(key)) };
    let value = json_string(&unescape(&key[1..close]));
    let suffix = &key[close + 1..];
    if let Some(lang) = suffix.strip_prefix('@') {
        format!("{{\"type\":\"literal\",\"value\":{value},\"xml:lang\":{}}}", json_string(lang))
    } else if let Some(dt) = suffix.strip_prefix("^^<").and_then(|d| d.strip_suffix('>')) {
        format!("{{\"type\":\"literal\",\"value\":{value},\"datatype\":{}}}", json_string(dt))
    } else {
        format!("{{\"type\":\"literal\",\"value\":{value}}}")
    }
}

/// Undoes N-Triples string escapes (`\t`, `\"`, `\uXXXX`, ...).
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(e @ ('u' | 'U')) => {
                let hex: String = chars.by_ref().take(if e == 'u' { 4 } else { 8 }).collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>s-store</title>
<style>
  :root { --bg: #fafafa; --fg: #1d1d1f; --muted: #6b6b70; --line: #d9d9de; --accent: #2557d6; --panel: #fff; }
  @media (prefers-color-scheme: dark) { :root { --bg: #161618; --fg: #e8e8ea; --muted: #9a9aa2; --line: #333338; --accent: #7aa2ff; --panel: #1f1f22; } }
  body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.45 system-ui, sans-serif; }
  main { max-width: 1100px; margin: 0 auto; padding: 16px; }
  h1 { font-size: 20px; margin: 8px 0 4px; } .muted { color: var(--muted); font-size: 13px; }
  section { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px; margin: 12px 0; }
  textarea { width: 100%; box-sizing: border-box; min-height: 140px; font: 13px/1.4 ui-monospace, monospace; background: var(--bg); color: var(--fg); border: 1px solid var(--line); border-radius: 6px; padding: 8px; }
  input[type=text] { background: var(--bg); color: var(--fg); border: 1px solid var(--line); border-radius: 6px; padding: 5px 8px; min-width: 0; }
  button { background: var(--accent); color: #fff; border: 0; border-radius: 6px; padding: 6px 14px; cursor: pointer; }
  .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-top: 8px; }
  .out { overflow: auto; max-height: 60vh; }
  table { border-collapse: collapse; font: 13px ui-monospace, monospace; }
  th, td { border-bottom: 1px solid var(--line); padding: 3px 10px 3px 0; text-align: left; vertical-align: top; white-space: pre-wrap; word-break: break-all; }
  pre { margin: 0; font: 13px ui-monospace, monospace; white-space: pre-wrap; }
</style>
</head>
<body>
<main>
  <h1>s-store</h1>
  <div class="muted" id="stats">…</div>
  <section>
    <textarea id="query">SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p ORDER BY DESC(?n) LIMIT 20</textarea>
    <div class="row"><button id="run">Run query</button><span class="muted" id="status"></span></div>
    <div class="out" id="results"></div>
  </section>
  <section>
    <strong>Load data</strong>
    <div class="row">
      <input type="file" id="file" accept=".nt,.ttl">
      <input type="text" id="graph" placeholder="named graph IRI (empty: default graph)" size="36">
      <button id="load">Load</button> <button id="compact" title="Repack the graphs' FSTs, reclaiming space uploads left behind">Compact</button><span class="muted" id="loadstatus"></span>
    </div>
  </section>
</main>
<script>
const $ = id => document.getElementById(id);
async function stats() {
  try { const s = await (await fetch('/stats')).json(); $('stats').textContent = `${s.triples.toLocaleString()} triples · ${(s.bytes / 1048576).toFixed(1)} MiB`; } catch (e) {}
}
function cell(t) {
  if (!t) return '';
  if (t.type === 'uri') return '<' + t.value + '>';
  if (t.type === 'bnode') return '_:' + t.value;
  return JSON.stringify(t.value) + (t['xml:lang'] ? '@' + t['xml:lang'] : t.datatype ? '^^<' + t.datatype + '>' : '');
}
async function run() {
  $('status').textContent = 'running…'; $('results').textContent = '';
  const t = performance.now();
  const r = await fetch('/sparql', { method: 'POST', headers: { 'Content-Type': 'application/sparql-query' }, body: $('query').value });
  const ms = (performance.now() - t).toFixed(1);
  const type = r.headers.get('Content-Type') || '';
  if (!r.ok) { $('status').textContent = 'error'; const pre = document.createElement('pre'); pre.textContent = await r.text(); $('results').append(pre); return; }
  if (type.includes('n-triples')) { const text = await r.text(); const pre = document.createElement('pre'); pre.textContent = text; $('results').append(pre); $('status').textContent = `${text.split('\n').filter(Boolean).length} triples · ${ms} ms`; return; }
  const data = await r.json();
  if ('boolean' in data) { $('results').textContent = String(data.boolean); $('status').textContent = `${ms} ms`; return; }
  const vars = data.head.vars, rows = data.results.bindings, shown = rows.slice(0, 1000);
  const table = document.createElement('table'), head = table.insertRow();
  vars.forEach(v => { const th = document.createElement('th'); th.textContent = '?' + v; head.append(th); });
  shown.forEach(b => { const tr = table.insertRow(); vars.forEach(v => { tr.insertCell().textContent = cell(b[v]); }); });
  $('results').append(table);
  $('status').textContent = `${rows.length} rows${rows.length > shown.length ? ' (first 1000 shown)' : ''} · ${ms} ms`;
}
async function load() {
  const f = $('file').files[0]; if (!f) return;
  const format = f.name.endsWith('.ttl') ? 'ttl' : 'nt';
  const params = new URLSearchParams({ format }); if ($('graph').value) params.set('graph', $('graph').value);
  $('loadstatus').textContent = 'loading…';
  const r = await fetch('/load?' + params, { method: 'POST', body: f });
  const text = await r.text();
  if (!r.ok) { $('loadstatus').textContent = 'error: ' + text; return; }
  const s = JSON.parse(text);
  $('loadstatus').textContent = `${s.triples.toLocaleString()} triples (${s.inserted.toLocaleString()} new) in ${s.ms.toFixed(0)} ms`;
  stats();
}
async function compact() {
  $('loadstatus').textContent = 'compacting…';
  const r = await fetch('/compact', { method: 'POST' });
  const text = await r.text();
  if (!r.ok) { $('loadstatus').textContent = 'error: ' + text; return; }
  $('loadstatus').textContent = `compacted in ${JSON.parse(text).ms.toFixed(0)} ms`;
  stats();
}
$('run').onclick = run; $('load').onclick = load; $('compact').onclick = compact;
$('query').addEventListener('keydown', e => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) run(); });
stats();
</script>
</body>
</html>
"#;
