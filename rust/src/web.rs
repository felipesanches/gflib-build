//! Web dashboard (`--ui web`) — a dependency-free HTTP/1.1 server (std `TcpListener`, one thread per
//! connection) that mirrors the TUI: it serves the snapshot at `/api/status` and routes live controls
//! (pause / retry) to control.json via `POST /api/control` — the same channel the curses monitor uses.
//! The browser page polls `/api/status` every 1.5 s and renders every tab. The page is intentionally
//! kept structurally parallel to the TUI (same tab + section order, colours, formats) so a user who
//! switches between the terminal and the browser sees the same thing. (See `WEB_UI_PLAN.md`.)

use crate::model::ControlSet;
use crate::monitor::Source;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn run(source: Arc<dyn Source>, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    eprintln!("gflib-build web dashboard: http://127.0.0.1:{}/", port);
    // Poll for connections so we can also notice a graceful shutdown (SIGTERM / UI "Restart") and return,
    // letting main() finalize + re-spawn. (A monitor never sets the flag, so its server keeps serving.)
    loop {
        if crate::daemon::sigterm_received() {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let src = Arc::clone(&source);
                std::thread::spawn(move || {
                    let _ = handle(stream, src);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

fn handle(mut stream: TcpStream, source: Arc<dyn Source>) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    // content-length (bounded) for POST bodies
    let mut content_len = 0usize;
    for l in req.lines() {
        let ll = l.to_ascii_lowercase();
        if let Some(v) = ll.strip_prefix("content-length:") {
            content_len = v.trim().parse::<usize>().unwrap_or(0).min(1 << 20);
        }
    }

    match (method, path) {
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/status") => {
            let snap = source.snapshot();
            let body = serde_json::to_vec(&snap).unwrap_or_else(|_| b"{}".to_vec());
            respond(&mut stream, 200, "application/json", &body)
        }
        ("GET", p) if p.starts_with("/api/log") => {
            // tail of a family's per-build log: /api/log?slug=ofl/foo&n=200
            let slug = query_param(p, "slug").unwrap_or_default();
            let n = query_param(p, "n").and_then(|s| s.parse::<usize>().ok()).unwrap_or(200).min(5000);
            let body = read_log_tail(&source.build_dir(), &slug, n);
            respond(&mut stream, 200, "text/plain; charset=utf-8", body.as_bytes())
        }
        ("GET", p) if p.starts_with("/api/fontspector") => {
            // one family's full fontspector result: /api/fontspector?slug=ofl/foo
            let slug = query_param(p, "slug").unwrap_or_default();
            let path = crate::persist::fontspector_dir(&source.build_dir())
                .join(format!("{}.json", slug.replace('/', "__")));
            let body = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
            respond(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("GET", p) if p.starts_with("/api/debian") => {
            // a drafted package's debian/ file contents: /api/debian?slug=ofl/foo
            let slug = query_param(p, "slug").unwrap_or_default();
            let body = read_debian_files(&source.build_dir(), &slug);
            respond(&mut stream, 200, "text/plain; charset=utf-8", body.as_bytes())
        }
        ("GET", p) if p.starts_with("/api/lintian") => {
            // the saved lintian report for a package: /api/lintian?slug=ofl/foo
            let slug = query_param(p, "slug").unwrap_or_default();
            let body = crate::deb::lintian_report(&source.build_dir(), &slug);
            respond(&mut stream, 200, "text/plain; charset=utf-8", body.as_bytes())
        }
        // "/api/deb?" (the trailing '?' disambiguates from "/api/debian"): download the built .deb
        ("GET", p) if p.starts_with("/api/deb?") => {
            let slug = query_param(p, "slug").unwrap_or_default();
            match crate::deb::deb_file(&source.build_dir(), &slug) {
                Some(path) => {
                    let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "package.deb".into());
                    match std::fs::read(&path) {
                        Ok(bytes) => respond_download(&mut stream, "application/vnd.debian.binary-package", &fname, &bytes),
                        Err(_) => respond(&mut stream, 404, "text/plain", b"deb not readable"),
                    }
                }
                None => respond(&mut stream, 404, "text/plain", b"deb not built for this family"),
            }
        }
        ("POST", "/api/control") => {
            // body may be partially read already; gather the rest up to content_len
            let body = read_body(&req, &mut stream, content_len);
            let ok = apply_control(&source, &body);
            let payload = if ok { b"{\"ok\":true}".to_vec() } else { b"{\"ok\":false}".to_vec() };
            respond(&mut stream, 200, "application/json", &payload)
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn read_body(req: &str, stream: &mut TcpStream, content_len: usize) -> String {
    // the body after the blank line that we already have in `req`
    let already = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    if already.len() >= content_len || content_len == 0 {
        return already;
    }
    let mut body = already.into_bytes();
    let mut extra = vec![0u8; content_len - body.len()];
    if let Ok(m) = stream.read(&mut extra) {
        body.extend_from_slice(&extra[..m]);
    }
    String::from_utf8_lossy(&body).to_string()
}

/// Package metadata for the detail panel: the .deb's own control + contents, then the source recipe.
fn read_debian_files(build_dir: &std::path::Path, slug: &str) -> String {
    crate::deb::package_metadata(build_dir, slug)
}

fn apply_control(source: &Arc<dyn Source>, body: &str) -> bool {
    // expects {"set": {...}} (same shape as control.json) OR a bare ControlSet
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let setv = v.get("set").cloned().unwrap_or(v);
    let set: ControlSet = match serde_json::from_value(setv) {
        Ok(s) => s,
        Err(_) => return false,
    };
    source.control(&set);
    true
}

/// Extract a query-string parameter from a request path (`/api/log?slug=…&n=…`), URL-decoded.
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split('?').nth(1)?;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next() == Some(key) {
            return Some(urldecode(it.next().unwrap_or("")));
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = |c: u8| (c as char).to_digit(16);
                if let (Some(a), Some(c)) = (h(b[i + 1]), h(b[i + 2])) {
                    out.push((a * 16 + c) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Last `n` lines of a family's per-build log. `slug.replace('/', "__")` leaves no path separator, so
/// the read can't escape `build_dir/logs/` (no traversal).
fn read_log_tail(build_dir: &std::path::Path, slug: &str, n: usize) -> String {
    if slug.is_empty() {
        return "(no slug)".into();
    }
    let path = build_dir.join("logs").join(format!("{}.log", slug.replace('/', "__")));
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let lines: Vec<&str> = t.lines().collect();
            let start = lines.len().saturating_sub(n);
            let tail = lines[start..].join("\n");
            if tail.is_empty() { "(log is empty)".into() } else { tail }
        }
        Err(_) => "(no log yet)".into(),
    }
}

/// Like `respond`, but with a Content-Disposition so the browser downloads the body as a file.
fn respond_download(stream: &mut TcpStream, ctype: &str, filename: &str, body: &[u8]) -> std::io::Result<()> {
    // strip any quote/CR/LF from the filename so it can't break out of the header
    let safe: String = filename.chars().filter(|c| !matches!(c, '"' | '\r' | '\n')).collect();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nContent-Disposition: attachment; filename=\"{}\"\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        ctype,
        body.len(),
        safe
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        404 => "404 Not Found",
        _ => "500 Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        status,
        ctype,
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// The single-page dashboard. Dependency-free: vanilla JS, no CDN/npm. Structurally parallel to the
// curses TUI — same tab order, same sections, same colours/formats. (W1 of WEB_UI_PLAN.md.)
const PAGE: &str = r###"<!doctype html><html><head><meta charset="utf-8">
<title>gflib-build dashboard</title>
<style>
 :root{--g:#86efac;--r:#fca5a5;--c:#67e8f9;--dc:#0e9bbd;--y:#fde68a;--o:#fb923c;--muted:#7c8aa0;--dr:#c77d7d;--w:#fff;--gr:#cbd5e1;--m:#f0abfc;--bg:#0b0e14;--panel:#11161f;--line:#1e293b;--secbg:#0e1420;--hover:#16202f;--pinbg:#1a1505}
 body[data-theme=light]{--g:#15803d;--r:#b91c1c;--c:#0e7490;--dc:#0e6d85;--y:#a16207;--o:#c2620a;--muted:#64748b;--dr:#b45454;--w:#0b1220;--gr:#1e293b;--m:#a21caf;--bg:#f6f7f9;--panel:#ffffff;--line:#e2e8f0;--secbg:#eef2f7;--hover:#e8edf3;--pinbg:#fdf6e3}
 body{background:var(--bg);color:var(--gr);font:13px/1.5 ui-monospace,Menlo,Consolas,monospace;margin:0;padding:10px 12px}
 /* W5: responsive multi-pane (side-by-side panels on wide screens — used by the packaging tab) */
 .panes{display:grid;grid-template-columns:1fr 1fr;gap:0 16px}
 @media(max-width:900px){.panes{grid-template-columns:1fr}}
 .g{color:var(--g)}.r{color:var(--r)}.c{color:var(--c)}.dc{color:var(--dc)}.y{color:var(--y)}.muted{color:var(--muted)}.dr{color:var(--dr)}.w{color:var(--w)}.gr{color:var(--gr)}.m{color:var(--m)}.o{color:var(--o)}.b{font-weight:600}
 .t{font-size:15px;color:var(--w);font-weight:600}
 .sub{color:var(--c)}
 .right{float:right}
 /* segmented progress bar */
 .barwrap{position:relative;height:18px;background:var(--line);border-radius:4px;overflow:hidden;margin:6px 0;display:flex}
 .seg{height:100%;display:flex;align-items:center;justify-content:center;overflow:hidden}.seg.bg{background:#22c55e}.seg.rg{background:#ef4444}.seg.dg{background:#334155}.seg.cg{background:#06b6d4}
 /* built-portion split by compiler — three shades of green: both=lightest/brightest, fontc=medium, fontmake=darkest */
 .seg.gfc{background:#22c55e}.seg.gfm{background:#15803d}.seg.gboth{background:#4ade80}
 .sl{font-size:10px;font-weight:600;color:#fff;text-shadow:0 0 3px #000;white-space:nowrap;padding:0 4px}.seg.dg .sl{color:#cbd5e1}
 .barlbl{position:absolute;left:0;right:0;top:0;line-height:18px;text-align:center;color:#fff;font-weight:600;font-size:11px;text-shadow:0 0 3px #000}
 .pkgbar{height:12px;margin:0 0 6px}.pkgbar .sl{font-size:9px}
 .phase{margin:4px 0 0}
 .skip{color:var(--y);float:right}
 /* tabs */
 .tabs{margin:8px 0 4px;border-bottom:1px solid var(--line);padding-bottom:4px}
 .tab{display:inline-block;padding:3px 11px;cursor:pointer;border-radius:4px 4px 0 0;color:var(--muted)}
 .tab.on{background:var(--line);color:var(--w)}
 .tabhint{float:right;color:var(--muted);font-size:11px;padding-top:4px}
 /* controls */
 .ctl{margin:4px 0 8px;color:var(--muted)}
 button{background:var(--line);color:var(--gr);border:1px solid #334155;border-radius:4px;padding:2px 9px;cursor:pointer;font:inherit}
 button:disabled{opacity:.4;cursor:default}
 .rb{visibility:hidden;margin-left:8px;padding:0 6px;font-size:11px;cursor:pointer;text-decoration:none;border:1px solid var(--line);border-radius:3px;background:var(--panel);color:var(--c);white-space:nowrap}
 .ln:hover .rb{visibility:visible}
 .dhead .rb{visibility:visible}
 .rb.r{color:var(--r);border-color:var(--r)}
 /* sections + rows */
 .sec{background:var(--secbg);border-left:3px solid var(--line);color:var(--w);font-weight:600;padding:3px 8px;margin:10px 0 2px;border-radius:0 4px 4px 0}
 .sec small{font-weight:400;color:var(--c);font-size:11px}
 .ln{white-space:pre;padding:1px 8px;border-radius:3px}
 .ln:hover{background:var(--hover)}
 .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(26ch,1fr));gap:0 8px;padding:2px 8px}
 .pin{background:var(--pinbg);border:1px solid var(--line);border-radius:6px;padding:4px 0;margin:6px 0}
 .pin .sec{background:none;border:none;color:var(--y);margin:2px 0}
 /* flow the now-building rows into as many columns as fit, so every in-flight family is shown without a cap */
 .bgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(64ch,1fr));gap:0 18px;align-items:start}
 .bgrid .ln{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
 .cfg td{padding:1px 10px 1px 8px;white-space:pre}
 /* charts (hand-rolled, dependency-free: CSS-div bars + inline-SVG donuts/rings) */
 .chartrow{display:flex;flex-wrap:wrap;gap:12px;margin:8px 0}
 .chart{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:10px 12px;flex:1;min-width:250px}
 .ctitle{color:var(--w);font-weight:600;margin-bottom:8px;font-size:12px}
 .bars{display:flex;flex-direction:column;gap:3px}
 .brow{display:flex;align-items:center;gap:6px}
 .blabel{width:36%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--gr);font-size:11px}
 .btrack{flex:1;background:var(--line);border-radius:3px;height:12px;overflow:hidden}
 .bfill{display:block;height:100%;border-radius:3px}
 .bval{width:66px;text-align:right;color:var(--muted);font-size:11px}
 .dwrap{display:flex;align-items:center;gap:12px;flex-wrap:wrap}
 .legend{font-size:11px;color:var(--gr)}
 .legend span{display:inline-block;margin:2px 10px 0 0}
 .legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:4px;vertical-align:middle}
 .clk{cursor:pointer}.clk:hover{background:var(--hover)}
 /* detail panel + config form */
 #detail{display:none;position:fixed;top:0;right:0;width:46%;max-width:780px;height:100%;overflow:auto;background:var(--panel);border-left:1px solid var(--line);box-shadow:-8px 0 24px rgba(0,0,0,.5);padding:12px 14px;z-index:50}
 .dhead{color:var(--w);font-weight:600;border-bottom:1px solid var(--line);padding-bottom:6px;margin-bottom:8px}
 .dclose{float:right;cursor:pointer;color:var(--muted);font-weight:400}
 .dbody{white-space:pre-wrap;word-break:break-word;font:inherit;margin:0;color:var(--gr)}
 .dlog{margin-top:10px;border-top:1px solid var(--line);padding-top:8px}
 .cfg input,.cfg select{background:var(--line);color:var(--w);border:1px solid var(--line);border-radius:4px;padding:1px 5px;font:inherit}
 .cfg input[type=number]{width:74px}
 /* W4: filter box + export toolbar */
 .toolbar{margin:6px 0;display:flex;gap:8px;align-items:center;flex-wrap:wrap}
 #filter{background:var(--line);color:var(--w);border:1px solid var(--line);border-radius:4px;padding:2px 8px;font:inherit;width:240px}
 .tbtn{font-size:11px}.tbtn.on{background:#334155;color:#fff}
 .rtxt{fill:var(--w)}
 /* fontspector QA panel */
 .fsbar{display:inline-flex;width:150px;height:11px;border-radius:3px;overflow:hidden;background:var(--line);vertical-align:middle;margin:0 8px}
 .fsg{height:100%}.bval2{font-size:11px}.fsexp{padding:1px 0 6px 12px}
</style></head><body>
<script>document.body.dataset.theme=localStorage.getItem('gf_theme')||'dark'</script>
<div id="hdr"></div>
<div id="bar"></div>
<div class="tabs" id="tabs"></div>
<div class="ctl" id="ctl"></div>
<div id="pin"></div>
<div id="body"></div>
<div id="detail"></div>
<script>
let snap={}, tab='overview';
// Optimistic overrides for the live config widgets: a control's just-entered value is shown immediately
// (no waiting for the control.json round-trip), keyed by field → {v:entered, base:value-before-the-edit}.
// Each is dropped the moment the server snapshot moves off `base` (poll()), so a server-side clamp still
// wins and nothing is pinned stale.
let OPT={};
// tab order MUST match the TUI's VIEWS
const TABS=['config','overview','queue','cohorts','archive','built','packaging','tools','failures','stats','fontspector','crater','reset'];
// colour a fontc_crater verdict token (magenta = fontc can't build it — the gold pairing on our built rows)
function craterCol(tok){if(!tok)return 'muted';if(tok=='fontc-fail'||tok=='both-fail'||tok=='src-miss')return 'm';if(tok=='fmake-fail')return 'muted';if(tok[0]=='~')return 'y';return 'c'}
// readable label for the fontc_crater verdict (was the cryptic "cr:<token>")
function craterLabel(t){return {'fontc-fail':'fontc fails','fmake-fail':'fontmake fails','both-fail':'both fail','src-miss':'no source','match':'fontc ok'}[t]||('fontc '+t)}
const CRATER_TIP='fontc_crater: how the Rust compiler (fontc) builds this family vs fontmake';
// Migration-milestone glossary (keep in sync with tui.rs MILESTONES + docs/migration-milestones.md)
const MILESTONES=[['M0','Measurement foundation — record compiler + exact version for every build attempt'],['M1','Full buildability — 100% of buildable families produce the expected fonts (any backend)'],['M2','fontc-gap map — every buildable family attempted with fontc, the result recorded'],['M3','fontc equivalence — fontc output equivalent to fontmake/shipped, at scale'],['M4','fontc majority — families that build correctly with fontc alone (no fontmake fallback)'],['M5','Python-free pipeline — Rust-native gftools-builder3, no Python pre-build or deps'],['M6','latest-fontc currency — the M4/M5 set re-validated on the latest fontc'],['M7','100% Rust — the whole library: latest fontc, equivalent output, zero Python']];
// fontspector status → colour class (FAIL/FATAL/ERROR red · WARN yellow · PASS green · SKIP/INFO grey)
function fsCls(s){return {PASS:'g',WARN:'y',FAIL:'r',FATAL:'r',ERROR:'r'}[s]||'muted'}
function fsColor(s){return {PASS:'#22c55e',WARN:'#eab308',FAIL:'#ef4444',FATAL:'#b91c1c',ERROR:'#ef4444',SKIP:'#475569',INFO:'#64748b'}[s]||'#475569'}
const TASK_MARK={done:'✅',failed:'❌',running:'🔄',skipped:'➖',pending:'⏳'};
const TASK_CLS={done:'g',failed:'r',running:'y',skipped:'muted',pending:'gr'};
// the full CONFIG_SCHEMA (display order), mirroring the TUI
const SCHEMA=[
 {k:'source',l:'worklist source',t:'choice',live:false},
 {k:'google_fonts',l:'google/fonts clone',t:'path',live:false},
 {k:'archive',l:'repo archive',t:'path',live:false},
 {k:'build_dir',l:'build output dir',t:'path',live:false},
 {k:'backend',l:'build backend',t:'choice',live:true},
 {k:'orchestrator',l:'orchestrator',t:'choice',live:true},
 {k:'fontc_bin',l:'fontc binary (override)',t:'path',live:false},
 {k:'auto_provision',l:'auto-provision pinned toolchain',t:'bool',live:false},
 {k:'jobs',l:'parallel jobs',t:'step',live:true},
 {k:'percent',l:'percent of library',t:'step',live:true},
 {k:'timeout',l:'per-build timeout (0=off)',t:'step',live:true},
 {k:'populate_archive',l:'populate archive (fetch repos)',t:'bool',live:true},
 {k:'manage_venvs',l:'cohort venvs',t:'bool',live:false},
 {k:'retry_failed',l:'retry ALL failed (incl. genuine errors)',t:'bool',live:false},
 {k:'auto_upgrade',l:'auto-upgrade built families (better backend)',t:'bool',live:false},
 {k:'compare',l:'compare to shipped',t:'bool',live:true},
 {k:'fontspector_qa',l:'fontspector QA on green builds',t:'bool',live:false},
 {k:'build_debs',l:'build .deb packages (auto-package built families)',t:'bool',live:true},
];
function human(n){n=n||0;const u=['B','KiB','MiB','GiB','TiB'];let i=0;while(n>=1024&&i<u.length-1){n/=1024;i++}return (i?n.toFixed(1):n)+u[i]}
function hms(s){s=Math.max(0,s|0);return [s/3600|0,(s%3600)/60|0,s%60].map(x=>String(x).padStart(2,'0')).join(':')}
function E(s){return (s==null?'':''+s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
function L(s,n){s=(s==null?'':''+s);return s.length>n?s.slice(0,n):s.padEnd(n)}
function Rp(s,n){return (''+s).padStart(n)}
function trunc(s,n){s=(s==null?'':''+s);return s.length>n?s.slice(0,n-1)+'…':s}
function prov(x){const c=x.compiler_version||x.backend||'';return c+(x.builder_version?' · '+x.builder_version:'')}
function ctl(set){fetch('/api/control',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({set:set})});
 for(const k in set)if(LIVE_APPLY[k])OPT[k]={v:set[k],base:(snap.config||{})[k]}; // show live-widget edits instantly
 bump()}
// --- reset tab: granular deletion of build-system portions (mirrors the TUI's reset tab) ---
function resetPortion(key,label,bytes){
 ctl({reset_portion:key}); // fires immediately; the row reports progress + '✓ freed X'
}
function resetView(){
 const ps=snap.reset_portions||[];
 let h='<div class="sec">Reset — delete a portion of the build system  <span class="muted">(items in use by a running build are kept · the bare git archive is never touched · deleting fonts re-queues those families so the progress bar regresses)</span></div>';
 if(!ps.length)return h+'<div class="ln muted">(sizes are being measured — they refresh every ~30 s)</div>';
 h+=ps.map(p=>{
  if(p.deleting){
   // live deletion: a progress bar + the remaining bytes counting down to zero
   const tot=p.bytes||1,fr=Math.min(p.freed||0,tot),pct=Math.floor(100*fr/tot),rem=tot-fr;
   return '<div class="ln"><button class="btn" disabled>deleting…</button>  <b>'+E(p.label)+'</b>  '+
    '<span class="y">'+human(rem)+' remaining</span>'+
    '<div class="barwrap" style="max-width:46em"><div class="seg rg" style="width:'+pct+'%"></div><div class="seg dg" style="width:'+(100-pct)+'%"></div>'+
    '<div class="barlbl">'+human(fr)+' / '+human(tot)+' deleted ('+pct+'%)</div></div></div>';
  }
  if(p.key=='all'){
   // the global nuke — danger-styled, behind a confirm (the only reset-tab action that asks)
   const note=p.note?'<span class="'+(p.note.indexOf('⛔')==0?'r':'g')+'"> — '+E(p.note)+'</span>':'';
   return '<div class="ln" style="margin:4px 0 10px">'+
     '<button class="btn" style="background:#b91c1c;border-color:#b91c1c;color:#fff;font-weight:600" onclick="if(confirm(\'Delete EVERYTHING? This STOPS all running jobs, PAUSES the build, and permanently deletes ALL build data (outputs, logs, venvs, packages, state, toolchain) — only the bare git repo archive is kept. Every family resets to queued. Continue?\'))resetPortion(\'all\',\'all\','+p.bytes+')">⚠ Delete everything!</button>  '+
     '<span class="y">'+human(p.bytes)+'</span>'+note+
     '<br><span class="muted" style="margin-left:1em">'+E(p.hint)+'</span></div>';
  }
  // enable on 'actionable' (frees disk / deletes a log / resets a result), not raw bytes — so a font
  // portion stays clickable when only logs or 'built' results remain. Fall back to bytes for old snapshots.
  const dis=(p.actionable===undefined)?(p.bytes==0):!p.actionable;
  const note=p.note?'<span class="'+(p.note.indexOf('\u26d4')==0?'r':'g')+'"> — '+E(p.note)+'</span>':'';
  const sub=p.note?'':('<br><span class="muted" style="margin-left:5.5em">'+E(p.hint)+'</span>');
  return '<div class="ln"><button class="btn" '+(dis?'disabled':'')+' onclick="resetPortion(\''+p.key+'\',\''+E(p.label)+'\','+p.bytes+')">delete</button>  '+
   '<b>'+E(p.label)+'</b>  <span class="'+(dis?'muted':'y')+'">'+human(p.bytes)+'</span>'+note+sub+'</div>';
 }).join('');
 h+='<div class="ln muted">outcomes are reported in the control log (config tab) — e.g. "reset venvs: freed 12.3GiB"</div>';
 return h;
}
function setTab(t){tab=t;location.hash=t;render()}
async function poll(){
 if(polling)return; // never overlap requests — this is the self-throttle that keeps polling from flooding the daemon
 polling=true;let txt='';
 // bound the request: a hung fetch (stale socket after suspend, half-open TCP) must never pin polling=true
 // and wedge the loop — on abort the catch falls through to polling=false + schedulePoll() and we recover.
 const ac=new AbortController(), tid=setTimeout(()=>ac.abort(), POLL_SLOW*3);
 try{const r=await fetch('/api/status',{signal:ac.signal});txt=await r.text();snap=JSON.parse(txt)}catch(e){}
 clearTimeout(tid);polling=false;
 if(txt&&txt!==lastSig){lastSig=txt;idleStreak=0}else{idleStreak++} // a changed snapshot keeps us fast; a stable one eases off
 for(const k in OPT)if((snap.config||{})[k]!==OPT[k].base)delete OPT[k]; // server moved off the pre-edit value → drop the override
 try{sample();render();checkNotify()}catch(e){} // a render error must never break the self-rescheduling loop
 schedulePoll();
}

// --- a row = {segs:[[text,class]…], rt?:retry-slug, det?:[kind,id], fc?:cause-to-filter-by} ---
let DET=[], fsCause=null;
function setFsCause(c){fsCause=(fsCause==c?null:c);render()}
function R(row){
 let cls='ln',oc='';
 if(row.fc!=null){cls='ln clk';oc=' onclick="setFsCause(\''+E(row.fc)+'\')"';}
 else if(row.det){DET.push(row.det);cls='ln clk';oc=' onclick="openDetailIdx('+(DET.length-1)+')"';}
 const sp=s=>'<span class="'+s[1]+'"'+(s[2]?' title="'+E(s[2])+'"':'')+'>'+E(s[0])+'</span>';
 // the retry button sits right after the FIRST seg (the family slug), not at the end of the row
 let h='<div class="'+cls+'"'+oc+'>'+(row.segs[0]?sp(row.segs[0]):'');
 if(row.rt) h+='<button class="rb" onclick="event.stopPropagation();ctl({retry:[\''+E(row.rt)+'\']})" title="retry this family">↻ retry</button>';
 // packaging actions: download the built .deb, and read the saved lintian report (lr=[slug,title,errClass])
 if(row.dl) h+='<a class="rb" href="'+E(row.dl)+'" download onclick="event.stopPropagation()" title="download the built .deb">⬇ .deb</a>';
 if(row.lr) h+='<button class="rb '+(row.lr[2]||'')+'" onclick="event.stopPropagation();openLintian(\''+E(row.lr[0])+'\')" title="'+E(row.lr[1])+'">▤ lintian</button>';
 h+=row.segs.slice(1).map(sp).join('');
 return h+'</div>';
}
// header format: "[n] Title <small>(trailing hint)</small>" — count up front, any trailing
// parenthetical rendered small/muted (titles whose paren isn't trailing stay inline)
function secHdr(title,n){const i=title.lastIndexOf(' (');
 const body=(i>=0&&title.endsWith(')'))?E(title.slice(0,i).trimEnd())+' <small>'+E(title.slice(i+1))+'</small>':E(title);
 return '<div class="sec">['+n+'] '+body+'</div>'}
function renderSec(s){return secHdr(s.title,s.rows.length)+(s.rows.length?s.rows.map(R).join(''):'<div class="ln muted">(none)</div>')}

// --- per-row builders (formats + colours match the TUI exactly; det = click-to-detail) ---
function taskRow(t){const m=TASK_MARK[t.status]||'?',cl=TASK_CLS[t.status]||'gr';
 const prog=t.total?(t.done+'/'+t.total):'',el=t.elapsed?hms(t.elapsed):'';
 return {segs:[[m+' '+L(t.name,26)+' '+L(prog,11)+Rp(el,8)+'  '+(t.detail||''),cl]],det:['task',t.key]}}
function failRow(f){const segs=[[L(f.slug,34)+' ','r']];if(f.rebuild_note)segs.push(['⟳ ','y']);if(f.crater)segs.push(['['+craterLabel(f.crater)+'] ',craterCol(f.crater),CRATER_TIP]);segs.push([f.error||'','dr']);return {segs,rt:f.slug,det:['failed',f.slug]}}
function qRow(q){const kc={retry:'y',rebuild:'c',upgrade:'m'}[q.kind]||'g';const segs=[['  '+L(q.kind,8)+' ',kc],[L(q.slug||'',38)+' ','gr']];if(q.crater)segs.push([craterLabel(q.crater),craterCol(q.crater),CRATER_TIP]);return {segs,rt:q.slug,det:['queue',q.slug]}}
// cohort member colour by build status: built=green, failed=red, building=yellow, else grey
function famCls(st){return {built:'g',failed:'r',building:'y'}[st]||'muted'}
function cohortRow(c){const segs=[[c.cached?'● ':'○ ',c.cached?'g':'muted'],[Rp(c.count,4)+'  '+L(c.key,14)+' ',!c.cached?'muted':(c.key=='base'?'w':'c')]];
 const f=c.families||[];if(!f.length)segs.push(['(no families yet)','muted']);else f.forEach((m,i)=>{if(i)segs.push([' | ','c']);segs.push([m.name,famCls(m.status)])});
 return {segs,det:['cohort',c.key]}}
function builtRow(b){const comp=b.compiler_version||b.backend||'';
 const segs=[[L(b.slug,32)+' ','g'],[L(comp,24)+' ','c'],[Rp(human(b.bytes),9)+'  '+L(b.compare||'',8),'gr']];
 if(b.crater)segs.push([' '+craterLabel(b.crater),craterCol(b.crater),CRATER_TIP]); // magenta here = we built what fontc can't
 return {segs,rt:b.slug,det:['built',b.slug]}}
// dpkg-deb validation: the control metadata parses (--info) AND the archive really contains a .ttf/.otf (--contents)
const DEB_VTIP='dpkg-deb checks: --info parses the control metadata and --contents lists a .ttf/.otf — the .deb is well-formed and actually contains fonts.';
function debStatus(b){const ds=b.deb_status||'',lint=b.deb_lint||'';
 if(ds=='lintian-fail')return ['lintian-fail','r','Validated by dpkg-deb, but lintian reported ERRORS — lintian: '+lint+'.'];
 if(ds=='lint-clean')return ['lint-clean','g b','Validated AND lintian clean (no errors or warnings). '+DEB_VTIP];
 if(ds=='lint-warn')return ['lint-warn','o','Validated, and lintian found NO errors — only warnings. lintian: '+lint+'.'];
 if(ds=='validated')return ['validated','dc',DEB_VTIP+((lint&&lint!='not run (lintian absent)')?' lintian: '+lint+'.':' (lintian has not run yet)')];
 if(ds=='built')return ['built','c','The .deb was produced, but it did NOT pass validation: the control failed to parse or the archive has no .ttf/.otf.'];
 if(ds=='no-fonts')return ['no fonts','muted','The fonts were discarded, so no .deb was packaged — not a failure. Re-run a build with .deb packaging enabled to keep the fonts.'];
 if(ds=='failed')return ['deb-failed','r','dpkg-deb failed to build the .deb for this family.'];
 if(b.packaged)return ['drafted','y','A debian/ packaging tree is drafted on disk; the .deb has not been built yet.'];
 return ['draftable','gr','Built family, ready to draft a debian/ tree (no debian/ on disk yet).'];}
function packagingRow(b){const comp=b.compiler_version||b.backend||'';const s=debStatus(b);const ds=b.deb_status||'',lint=b.deb_lint||'';
 const built=(ds=='built'||ds=='validated'||ds=='lint-clean');
 const dl=built?('/api/deb?slug='+encodeURIComponent(b.slug)):null; // download icon only when a .deb exists
 // a saved lintian report exists whenever lintian actually ran (clean / warnings / errors); flag errors red
 const hasReport=lint&&lint!='not run (lintian absent)'&&lint!='lintian failed to run';
 const lr=hasReport?[b.slug,'read the lintian report ('+lint+')',/error/.test(lint)?'r':'']:null;
 return {segs:[[L(s[0],10)+' ',s[1],s[2]],[L(b.slug,32)+' ','gr'],[L(comp,26)+' ','c'],[Rp(human(b.bytes),9),'gr']],rt:b.slug,dl:dl,lr:lr,det:['package',b.slug]}}
function toolRow(t){const rust=t.lang=='rust';
 return {segs:[[L(t.lang,7)+' ',rust?'g':'y'],[L(t.name,24)+' ','w'],[L(t.kind,12)+' ','c'],[Rp(t.families,4)+' families  ','gr'],[(t.packaged?'packaged':'unpackaged'),'gr']],det:['tool',t.name]}}
function debToolRow(t){const ok=!!t.present;
 return {segs:[[ok?'✓ ':'✗ ',ok?'g':'r'],[L(t.name,20)+' ','w'],[ok?(t.purpose||''):('MISSING — sudo apt install '+(t.provides||'')),ok?'gr':'y']]}}
// clicking a cause FILTERS the families list below (and highlights the selected cause)
function failcatRow(c){const sel=fsCause==c.cat;
 return {segs:[[(sel?'▸':' ')+Rp(c.count,3)+'  ','w'],[L(c.cat,24),sel?'y':'c'],[' '+(c.hint||''),'muted']],fc:c.cat}}
function histRow(h){return {segs:[[L(h.cause,20)+' ','y'],[h.slug||'','gr']],rt:h.slug,det:['history',h.slug]}}
function phaseRow(kv){return {segs:[[L(kv[0],12)+' '+hms(kv[1]),'gr']]}}
function opRow(kv){const s=kv[1];return {segs:[[L(kv[0],10)+' total '+Rp((s.total||0).toFixed(1),9)+'  n '+Rp(s.count||0,5)+'  mean '+Rp((s.mean||0).toFixed(2),7)+'  max '+Rp((s.max||0).toFixed(1),7),'c']]}}
function buildingRow(b){const note=b.note||b.backend||'';
 // frozen builds (job limit lowered) are SIGSTOP-paused → [FROZEN] in cyan so it's clear they aren't actively compiling
 if(b.frozen)return {segs:[['w'+Rp(b.worker,2)+' [FROZEN] '+L(b.slug,26)+' '+Rp(hms(b.dur),8)+'  '+note,'c']],det:['building',b.slug]};
 // an install (pip) over the lowered job limit / pause can't be frozen mid-stream — it WILL start frozen the
 // moment it reaches the compile step, so flag it (magenta) as draining toward that.
 const overLimit=snap.paused||(((snap.building||[]).length-(snap.frozen_builds||0))>(snap.jobs||1));
 if(note==='installing deps'&&overLimit)
  return {segs:[['w'+Rp(b.worker,2)+' '+L(b.slug,30)+' '+Rp(hms(b.dur),8)+'  [finishing install before freezing]','m']],det:['building',b.slug]};
 return {segs:[['w'+Rp(b.worker,2)+' '+L(b.slug,34)+' '+Rp(hms(b.dur),8)+'  '+note,'y']],det:['building',b.slug]}}

function sections(t){
 if(t=='overview')return [{title:'Pipeline',rows:(snap.tasks||[]).map(taskRow)},{title:'Recent failures',rows:filterList(snap.failures_recent,['slug','error']).map(failRow)}];
 if(t=='queue')return [{title:'Queued — priority order (re-queued families first, then longest previous build first)',rows:filterList(snap.queued_list,['slug','kind']).map(qRow)}];
 if(t=='cohorts')return [{title:'Dependency cohorts  (● = venv cached on disk)',rows:filterList(snap.cohorts,['key']).map(cohortRow)}];
 if(t=='built')return [{title:'Built — successes  (slug · compiler+version · size · vs-shipped)',rows:filterList(snap.built_recent,['slug','compiler_version','backend']).map(builtRow)}];
 if(t=='packaging')return [{title:'Deb toolchain  (install any ✗ to enable deb building/validation — auto-detected, recovers in ~5s)',rows:(snap.deb_tools||[]).map(debToolRow)},{title:'Packaging — per-family status  (drafted = debian/ on disk · draftable = built, ready to draft)',rows:filterList(snap.packages,['slug','compiler_version','backend']).map(packagingRow)}];
 if(t=='tools')return [{title:'Build-tool packages  (python = M5 blocker · rust = native · click = which families need it)',rows:filterList(snap.tool_packages,['name','lang','kind']).map(toolRow)},{title:'Migration milestones (M0–M7) — what the rungs mean',rows:MILESTONES.map(m=>({segs:[[L(m[0],4),'c'],[m[1],'gr']]}))}];
 if(t=='failures'){const s=[];const cats=snap.fail_categories||[];
  if(cats.length)s.push({title:'Failures by cause (click to filter)',rows:filterList(cats,['cat','hint']).map(failcatRow)});
  // families list, scoped to the selected cause. When a cause is selected we list its
  // OWN families (the authoritative full set behind the count) rather than intersecting
  // with the capped 'recent' window — otherwise a cause whose families fell out of that
  // window shows "(none)" despite a non-zero count. Errors: recent first, then history.
  let fr=snap.failures_recent||[], ftitle='Failures — newest first (current)';
  if(fsCause){const cat=cats.find(c=>c.cat==fsCause);const fams=cat?(cat.families||[]):[];
   const recent=snap.failures_recent||[], hist=snap.failure_history||[];
   const errFor=(slug)=>{const r=recent.find(f=>f.slug==slug);if(r&&r.error)return r.error;
    for(let i=hist.length-1;i>=0;i--)if(hist[i].slug==slug&&hist[i].error)return hist[i].error;return '';};
   fr=fams.map(slug=>({slug,error:errFor(slug)}));
   ftitle='Families failed — cause: '+fsCause+' ('+(cat?cat.count:fr.length)+', click the cause again to clear)';}
  s.push({title:ftitle,rows:filterList(fr,['slug','error']).map(failRow)});
  if((snap.failure_history||[]).length)s.push({title:'Failure history (persistent — survives restarts & re-attempts)',rows:filterList(snap.failure_history,['slug','cause','error']).map(histRow)});return s}
 if(t=='stats'){const ph=Object.entries(snap.phase_durations||{}).sort((a,b)=>b[1]-a[1]);
  const ops=Object.entries(snap.op_stats||{}).sort((a,b)=>(b[1].total||0)-(a[1].total||0));
  return [{title:'Phase timing',rows:ph.map(phaseRow)},{title:'Operation timing',rows:ops.map(opRow)}]}
 return [];
}

function statsPrefix(){const m=snap.migration||{};
 let line='builder3(M5: Python-free) '+(m.builder3||0)+'   fontc '+(m.fontc||0)+'   fontmake-fallback(blockers) '+(m.fontmake_fallback||0)+'   fontmake-only '+(m.fontmake_only||0);
 if((m.both_identical||0)||(m.both_differ||0))line+='   both id '+(m.both_identical||0)+'/diff '+(m.both_differ||0);
 let h='<div class="sec">fontc migration</div><div class="ln g">'+E(line)+'</div>';
 const tl=snap.tooling||{},bl=snap.builders||{};
 if(Object.keys(tl).length)h+='<div class="ln c">compilers in use:  '+Object.entries(tl).map(e=>E(e[0]+' → '+e[1])).join('   ')+'</div>';
 if(Object.keys(bl).length)h+='<div class="ln c">builders in use:   '+Object.entries(bl).map(e=>E(e[0]+' → '+e[1])).join('   ')+'</div>';
 const pv=snap.python_versions||{};
 if(Object.keys(pv).length)h+='<div class="ln g">Python interpreters: '+Object.entries(pv).sort().map(e=>E(e[0]+' → '+e[1]+' families')).join('   ')+'</div>';
 return h;
}

function archiveView(){const a=snap.archive||{};
 const unreach=(a.recent||[]).filter(r=>r.status=='failed');
 const added=(a.recent||[]).filter(r=>r.status!='failed').map(r=>r.repo);
 let h='<div class="ln c"> '+(a.total||0)+' repos mirrored on disk</div>';
 h+='<div class="ln gr">  '+(a.active||[]).length+' cloning now   '+(a.pending_total||0)+' queued   '+unreach.length+' unreachable</div>';
 const block=(items,cls,label)=>items.length?secHdr(label,items.length).replace(' ('+items.length+')','')+'<div class="grid">'+items.map(s=>'<span class="'+cls+'">'+E(s)+'</span>').join('')+'</div>':'';
 let g='';
 g+=block(a.active||[],'y','cloning now');
 g+=block(added,'g','recently archived (last 30 min)');
 g+=block(a.pending||[],'c','queued next');
 g+=block(unreach.map(r=>r.repo+' ('+trunc(r.reason||'',16)+')'),'r','unreachable (git reason)');
 if(!g)g='<div class="ln muted">(archive idle — nothing being mirrored)</div>';
 return h+g;
}

// ---- fontspector QA breakdown (panel A: families · panel B: checks-across-families) ----
let fsTab='checks';
function setFsTab(t){fsTab=t;render()}
// a small stacked PASS/WARN/FAIL/SKIP bar for a counts object
function fsBar(c){const o=['pass','warn','fail','fatal','error','skip','info'];const tot=o.reduce((a,k)=>a+(c[k]||0),0)||1;
 const seg=(k,col)=>(c[k]||0)?'<span class="fsg" style="width:'+(100*(c[k]||0)/tot)+'%;background:'+col+'" title="'+k+' '+(c[k]||0)+'"></span>':'';
 return '<span class="fsbar">'+seg('pass','#22c55e')+seg('info','#64748b')+seg('skip','#475569')+seg('warn','#eab308')+seg('fail','#ef4444')+seg('fatal','#b91c1c')+seg('error','#ef4444')+'</span>';}
function fsCount(c){const f=(c.fail||0)+(c.fatal||0)+(c.error||0);return (f?'<span class="r">'+f+' fail</span> · ':'')+(c.warn?'<span class="y">'+c.warn+' warn</span> · ':'')+'<span class="g">'+(c.pass||0)+' pass</span>'+((c.skip||0)?' · <span class="muted">'+c.skip+' skip</span>':'');}
function fsView(){const fs=snap.fontspector;
 if(!fs)return '<div class="sec">fontspector QA</div><div class="ln muted">No QA results yet. Run a pass:  gflib-build --fontspector  (then refresh).</div>';
 const t=fs.total||{},when=fs.ts?new Date(fs.ts*1000).toLocaleString():'';
 // outcome donut over the grand totals
 const sl=[{label:'pass',value:t.pass||0,color:'#22c55e'},{label:'warn',value:t.warn||0,color:'#eab308'},{label:'fail',value:(t.fail||0)+(t.fatal||0),color:'#ef4444'},{label:'error',value:t.error||0,color:'#b91c1c'},{label:'skip',value:t.skip||0,color:'#475569'},{label:'info',value:t.info||0,color:'#64748b'}];
 let h='<div class="chartrow">'+chartCard(E(fs.version)+' · profile '+E(fs.profile)+' · '+fs.families_checked+' families · '+when,'<div class="dwrap">'+donut(sl,52)+legend(sl)+'</div>')+'</div>';
 h+='<div class="toolbar"><button class="tbtn'+(fsTab=='checks'?' on':'')+'" onclick="setFsTab(\'checks\')">by check (across families)</button> '+
   '<button class="tbtn'+(fsTab=='families'?' on':'')+'" onclick="setFsTab(\'families\')">by family</button></div>';
 if(fsTab=='checks'){
  const checks=filterList(fs.per_check||[],['id','title']);
  h+='<div class="sec">Checks — most failures first ('+checks.length+')</div>';
  h+=checks.map((c,i)=>'<div class="ln clk" onclick="toggleFsCheck('+i+')"><span class="w">'+E(L(c.id,40))+'</span> '+fsBar(c.counts)+' <span class="bval2">'+fsCount(c.counts)+'</span></div>'+
    '<div id="fsck'+i+'" style="display:none" class="fsexp">'+
     (c.fail_families&&c.fail_families.length?'<div class="ln r">  FAIL: '+c.fail_families.map(s=>fsFamLink(s)).join(', ')+'</div>':'')+
     (c.warn_families&&c.warn_families.length?'<div class="ln y">  WARN: '+c.warn_families.map(s=>fsFamLink(s)).join(', ')+'</div>':'')+
     '<div class="ln muted">  '+E(c.title)+'</div></div>').join('');
 } else {
  const fams=filterList(fs.per_family||[],['slug']);
  h+='<div class="sec">Families — worst status first ('+fams.length+')</div>';
  h+=fams.map(f=>'<div class="ln clk" onclick="openDetail(\'fsfamily\',\''+E(f.slug)+'\')"><span class="'+fsCls(f.worst)+'">'+E(L(f.slug,40))+'</span> '+fsBar(f.counts)+' <span class="bval2">'+fsCount(f.counts)+'</span></div>').join('');
 }
 return h;
}
function fsFamLink(slug){return '<span class="clk c" onclick="event.stopPropagation();openDetail(\'fsfamily\',\''+E(slug)+'\')">'+E(slug)+'</span>'}
function toggleFsCheck(i){const e=document.getElementById('fsck'+i);if(e)e.style.display=e.style.display=='none'?'block':'none'}

function craterView(){const cv=snap.crater;
 if(!cv)return '<div class="sec">fontc_crater comparison</div><div class="ln muted">Not loaded. Put fontc_crater_targets.json in gflib-data (or run gfonts_agents’ fetch_crater_analysis.py), then refresh. --no-crater disables.</div>';
 const partial=cv.complete?'':' <span class="y">[PARTIAL: diff-only fallback — run fetch_crater_analysis.py for the fontc/both-failed split]</span>';
 let h='<div class="sec">fontc_crater comparison — summary'+partial+'</div>';
 h+='<div class="ln muted">crater run '+E(cv.run)+' · fontc '+E((cv.fontc_rev||'').slice(0,12))+' · google/fonts '+E((cv.fonts_repo_sha||'').slice(0,12))+' · matched '+(cv.matched||0)+' families</div>';
 h+='<div class="ln m b">  GOLD  we build · fontc can’t : '+(cv.we_build_fontc_cant||0)+'   (upstream-worthy build fixes)</div>';
 h+='<div class="ln r">  REGR  we fail  · fontc built : '+(cv.we_fail_fontc_ok||0)+'   (our build bugs)</div>';
 h+='<div class="ln g">  both build · identical       : '+(cv.both_ok_identical||0)+'</div>';
 h+='<div class="ln y">  both build · output differs  : '+(cv.both_ok_diff||0)+'</div>';
 h+='<div class="ln muted">crater verdicts (matched): match '+(cv.c_identical||0)+' · diff '+(cv.c_diff||0)+' · fontc-fail '+(cv.c_fontc_failed||0)+' · fmake-fail '+(cv.c_fontmake_failed||0)+' · both-fail '+(cv.c_both_failed||0)+' · src-miss '+(cv.c_repo_failed||0)+'</div>';
 const gold=cv.gold_families||[],regr=cv.regression_families||[];
 h+='<div class="sec">GOLD — we build, fontc_crater’s fontc cannot ('+(cv.we_build_fontc_cant||0)+')</div>';
 h+='<div class="ln muted">refresh this set with:  gflib-build --retrigger-crater fontc-failed</div>';
 h+=gold.length?gold.map(s=>'<div class="ln clk" onclick="openDetail(\'built\',\''+E(s)+'\')"><span class="m">'+E(L(s,50))+'</span> <span class="muted">fontc can’t build this — our build fix is upstream-worthy</span></div>').join(''):'<div class="ln muted">(none)</div>';
 h+='<div class="sec">Regressions — fontc builds it, we fail ('+(cv.we_fail_fontc_ok||0)+')</div>';
 h+=regr.length?regr.map(s=>'<div class="ln clk" onclick="openDetail(\'failed\',\''+E(s)+'\')"><span class="r">'+E(L(s,50))+'</span> <span class="muted">fontc builds this but we don’t — likely our bug</span></div>').join(''):'<div class="ln muted">(none)</div>';
 return h;
}

function showIf(k,cf){const s=x=>(cf[x]==null?'':''+cf[x]);
 if(k=='google_fonts')return s('source')=='metadata';
 if(k=='fontc_bin')return s('backend')!='fontmake';
 if(k=='auto_provision')return !s('fontc_bin');
 if(k=='compare')return s('source')=='metadata';
 return true}
const CHOICES={source:['metadata','archive'],backend:['auto','fontc','fontmake','both'],orchestrator:['auto','builder3','builder2']};
// the keys the daemon actually honours live (same set as the TUI's cfg_apply_live) → editable form controls
const LIVE_APPLY={backend:1,orchestrator:1,jobs:1,percent:1,compare:1,build_debs:1};
// logical groupings for the config panel (related settings under one sub-header)
const GROUPS=[
 {t:'Sources & paths', k:['source','google_fonts','archive','build_dir']},
 {t:'Build engine',    k:['backend','orchestrator','fontc_bin','auto_provision','manage_venvs','jobs','timeout']},
 {t:'Scope',           k:['percent','retry_failed','auto_upgrade','populate_archive']},
 {t:'QA & packaging',  k:['compare','fontspector_qa','build_debs']},
];
// one form control per field: live → an editable widget that posts straight to control.json;
// otherwise a real (greyed) widget that shows the current value but isn't editable on a running build.
function cfgCell(f,cf){
 const v=cf[f.k], live=LIVE_APPLY[f.k];
 if(f.t=='bool') // a REAL checkbox (disabled+greyed when not live); its state alone conveys the value
  return '<input type="checkbox"'+(v?' checked':'')+(live?' onchange="ctl({'+f.k+':this.checked})"':' disabled')+'>';
 if(f.t=='choice'){const ch=CHOICES[f.k]||[];
  if(live)return '<select onchange="ctl({'+f.k+':this.value})">'+ch.map(o=>'<option'+(o==v?' selected':'')+'>'+E(o)+'</option>').join('')+'</select>';
  return '<span class="muted">'+E(ch.includes(v)?v:(ch[0]||''))+'</span>';}
 if(live&&f.t=='step')
  return '<input type="number"'+(f.k=='percent'?' min="1" max="100"':' min="1"')+' value="'+E(v==null?'':v)+'" onchange="ctl({'+f.k+':+this.value})">';
 return '<span class="muted">'+E(v==null?(f.k=='timeout'?'0':''):''+v)+'</span>';
}
function cfgView(){const base=snap.config||{},cf={};
 for(const k in base)cf[k]=base[k];
 for(const k in OPT)cf[k]=OPT[k].v; // prefer a just-entered value over the (briefly stale) server snapshot
 let h='<div class="sec">Configuration</div>';
 h+='<div class="ln muted">Editable controls apply live; greyed ones are set by CLI flags at launch.</div>';
 h+='<div class="ln"><button class="tbtn" onclick="if(confirm(\'Restart the daemon now? In-flight builds are interrupted and resume from saved state.\'))ctl({restart:true})">↻ Restart daemon</button></div>';
 GROUPS.forEach(g=>{
  const fs=SCHEMA.filter(f=>g.k.includes(f.k)&&showIf(f.k,cf));
  if(!fs.length)return;
  h+='<div class="sec">'+E(g.t)+'</div><table class="cfg">';
  fs.forEach(f=>{h+='<tr><td class="w">'+E(f.l)+'</td><td>'+cfgCell(f,cf)+'</td></tr>';});
  h+='</table>';
 });
 const dr=snap.dep_relaxations||[];
 if(dr.length)h+='<div class="sec">auto-fixed dependencies (no manual pinning needed)</div>'+dr.map(l=>'<div class="ln y">'+E(l)+'</div>').join('');
 const cl=snap.control_log||[];
 if(cl.length)h+='<div class="sec">applied live changes</div>'+cl.slice().reverse().map(l=>'<div class="ln g">'+E(l)+'</div>').join('');
 return h;
}

// Apply fresh HTML to a region, but YIELD to the user: if the focused element (a field being typed
// in, or an OPEN <select>, which keeps focus while its list is dropped) lives inside this region, skip
// the rebuild for this frame so a background sync never steals focus, snaps a combo-box shut, or
// resets a half-entered value. It catches up on the next frame, once the interaction ends.
function setHTML(id,html){const el=document.getElementById(id);if(!el)return;
 const a=document.activeElement;
 if(a&&el.contains(a)&&/^(INPUT|SELECT|TEXTAREA)$/.test(a.tagName))return;
 el.innerHTML=html;}

function render(){
 const c=snap.counts||{};
 const pre=snap.pre_build;
 DET=[]; // reset the click-to-detail index map for this frame
 // ---- header (rows 0/1) ----
 let hdr='<div class="t"> Google Fonts library build'+(snap.paused?(snap.running_builds>0?' [PAUSED · '+snap.running_builds+(snap.running_builds==1?' build':' builds')+' frozen]':' [PAUSED]'):'')+
   (pre?'<span class="right muted">first-time setup</span>':'<span class="right w">elapsed '+hms(snap.elapsed)+'</span>')+'</div>';
 if(pre){hdr+='<div class="sub"> configure your build below, then navigate to ▶ Start build</div>';}
 else{
  const bld=snap.disk_build_total||0,arc=snap.disk_archive_total||0;
  const disk=snap.disk_archive_nested?('disk used '+human(bld)+' (build + nested archive, all included)')
    :('disk used '+human(bld+arc)+' (build '+human(bld)+' + archive '+human(arc)+')');
  // overall worklist progress, parked at the top-right under the elapsed clock (per-segment bars carry the breakdown)
  const att=(c.built||0)+(c.failed||0),insc=Math.max(1,att+(c.queued||0)+(c.building||0));
  const attLbl=(att>0||(c.queued||0)>0||(c.building||0)>0)?'<span class="right w">'+att+'/'+insc+' attempted ('+Math.floor(100*att/insc)+'%)</span>':'';
  hdr+='<div class="sub"> '+disk+'  free '+human(snap.disk_free)+attLbl+'</div>';
 }
 setHTML('hdr',hdr);
 // ---- progress bar (rows 2/3) ----
 setHTML('bar',pre?'':barHTML());
 // ---- tabs (row 4) ----
 setHTML('tabs',TABS.map(t=>'<span class="tab'+(t==tab?' on':'')+'" onclick="setTab(\''+t+'\')">'+t+'</span>').join(''));
 // ---- controls + W4 toolbar (filter on list tabs, export everywhere). setHTML() yields while the
 //      user is typing in the filter, so the live sync never steals focus from it. No refresh-rate
 //      knob: the page refreshes automatically (see the W5 polling block). ----
 {
  const listTab=['overview','queue','cohorts','built','failures','fontspector'].includes(tab);
  const notifyBtn=(window.Notification&&Notification.permission!='granted')?' <button class="tbtn" onclick="askNotify()" title="notify when the build completes">🔔 notify</button>':'';
  setHTML('ctl',
    '<button title="pause scheduling AND freeze (SIGSTOP) running builds to free CPU/RAM" onclick="ctl({paused:true})"'+(snap.paused?' disabled':'')+'>pause</button> '+
    '<button title="thaw (SIGCONT) frozen builds and resume scheduling" onclick="ctl({paused:false})"'+(snap.paused?'':' disabled')+'>resume</button>'+
    ' <button class="tbtn" title="force-rebuild ALL config-fixed families now (failed families with a gflib-build override). Editing an override already auto-rebuilds it within seconds; this button forces the whole set immediately, even unchanged ones." onclick="if(confirm(\'Force-rebuild all failed families that have a gflib-build config override (the config-fixed set)?\'))ctl({retry_overrides:true})">↻ rebuild config-fixed</button>'+
    (listTab?' <input id="filter" placeholder="filter… (slug / cause)" oninput="setFilter(this.value)" value="'+E(FILTER)+'">':'')+
    ' <button class="tbtn" onclick="exportJSON()">⬇ JSON</button> <button class="tbtn" onclick="exportCSV()">⬇ CSV (built+failed)</button>'+
    ' <button class="tbtn" onclick="toggleTheme()" title="light / dark">◐</button>'+notifyBtn+
    '<span class="muted"> &nbsp; click a row for details · updates live</span>');
 }
 // ---- pinned now-building (every tab) ----
 const bl=snap.building||[];let pin='';
 if(bl.length&&!pre){
  // Break down by STAGE: only the compile stage can be paused/frozen; venv-install ("installing deps")
  // + checkout can't — which is why pausing/lowering jobs doesn't visibly freeze families still installing.
  const tot=bl.length,comp=Math.min(snap.running_builds||0,tot),fz=Math.min(snap.frozen_builds||0,comp),oth=Math.max(0,tot-comp);
  const ps=[];if(comp>fz)ps.push((comp-fz)+' compiling');if(fz>0)ps.push(fz+' frozen');if(oth>0)ps.push(oth+' installing/setup');
  const lbl=ps.length>1?(tot+' — '+ps.join(', ')):(''+tot);
  // show ALL in-flight families (no '+N more' cap) — flow them into columns so the pinned block stays compact
  pin='<div class="pin"><div class="sec">▶ Now building ('+lbl+')</div><div class="bgrid">'+bl.map(b=>R(buildingRow(b))).join('')+'</div></div>';}
 setHTML('pin',pin);
 // ---- body per tab: charts (web-only) first, then the same content as the TUI ----
 let body=charts(tab);
 if(tab=='config')body+=cfgView();
 else if(tab=='archive')body+=archiveView();
 else if(tab=='fontspector')body+=fsView();
 else if(tab=='crater')body+=craterView();
 else if(tab=='packaging')body+=packagingView();
 else if(tab=='reset')body+=resetView();
 else if(tab=='overview')body+=sections(tab).map(renderSec).join(''); // stacked, like the TUI: Pipeline on top, Recent failures below
 else{body+=(tab=='stats'?statsPrefix():'')+sections(tab).map(renderSec).join('');}
 setHTML('body',body);
}

function barHTML(){const c=snap.counts||{},ph=snap.phase;
 // phase_error (ERR …) is shown for ALL phases (matches the TUI's check outside the if/else)
 const err=(snap.phase_error)?'<span class="skip r">ERR '+E((snap.phase_error||'').slice(0,24))+'</span>':'';
 // archive/cohorts phase-progress bar (cyan, non-segmented)
 if((ph=='archive'||ph=='cohorts')&&(snap.phase_total||0)>0){
  const frac=(snap.phase_done||0)/(snap.phase_total||1),pct=Math.floor(100*frac);
  return '<div class="phase y"> Phase: '+E(phaseLabel(ph))+'  '+(snap.phase_done||0)+'/'+(snap.phase_total||0)+'  '+E((snap.phase_label||'').slice(0,30))+err+'</div>'+
   '<div class="barwrap"><div class="seg cg" style="width:'+(100*frac)+'%"></div><div class="seg dg" style="width:'+(100-100*frac)+'%"></div>'+
   '<div class="barlbl">'+(snap.phase_done||0)+'/'+(snap.phase_total||0)+' '+E(phaseLabel(ph))+' ('+pct+'%)</div></div>';
 }
 // building: segmented bar over the IN-SCOPE worklist (excludes skipped)
 const inscope=Math.max(1,(c.built||0)+(c.failed||0)+(c.queued||0)+(c.building||0)),done=(c.built||0)+(c.failed||0);
 const gw=100*(c.built||0)/inscope,rw=100*(c.failed||0)/inscope,dw=Math.max(0,100-gw-rw);
 const rem=Math.max(0,inscope-done); // queued + building (the dark remainder)
 const hint=(c.skipped||0)?'<span class="skip">'+(c.skipped||0)+' skipped (not selected — raise % to 100 to build them)</span>':'';
 // each segment carries its own count + share-of-total label (hidden by overflow when the segment is too narrow)
 const seg=(w,cl,n,lbl)=>'<div class="seg '+cl+'" style="width:'+w+'%">'+(n>0?'<span class="sl">'+n+' '+lbl+' ('+Math.round(w)+'%)</span>':'')+'</div>';
 // split the built (green) portion by compiler, ordered as a pipeline toward the end-goal:
 // both · fontc · fontmake (three shades of green, brightest→darkest left to right),
 // plus a base-green remainder for any built family with an unrecorded backend
 const bk=snap.backends||{},bfc=bk.fontc||0,bfm=bk.fontmake||0,bb=bk.both||0,bother=Math.max(0,(c.built||0)-bfc-bfm-bb);
 const builtSegs=seg(100*bb/inscope,'gboth',bb,'both')+seg(100*bfc/inscope,'gfc',bfc,'fontc')
   +seg(100*bfm/inscope,'gfm',bfm,'fontmake')+(bother>0?seg(100*bother/inscope,'bg',bother,'built'):'');
 // the built/failed/building/queued counts now live in the per-segment bar labels + the top-right 'attempted'
 return '<div class="phase"> Phase: '+E(phaseLabel(ph))+err+hint+'</div>'+
  '<div class="barwrap">'+builtSegs+seg(rw,'rg',c.failed||0,'failed')+seg(dw,'dg',rem,'left')+'</div>'+
  (snap.build_debs?packagingBar(gw):'');  // packaging bar only when the .deb option is active
}
function phaseLabel(ph){return {init:'starting…',clone_gf:'cloning google/fonts',build_fontc:'building fontc from source',discover:'discovering worklist',archive:'populating archive (mirroring repos)',cohorts:'scanning dependency cohorts',build:'building',done:'done'}[ph]||ph||''}

// ---- W4: client-side timeseries (accumulated while the page is open; resets on reload) ----
let HIST=[];
function sample(){const c=snap.counts||{};const disk=(snap.disk_build_total||0)+(snap.disk_archive_total||0);
 const s={t:snap.elapsed||0,built:c.built||0,failed:c.failed||0,queued:c.queued||0,building:c.building||0,disk:disk};
 const last=HIST[HIST.length-1];
 if(!last||last.t!=s.t||last.built!=s.built||last.failed!=s.failed||last.disk!=s.disk)HIST.push(s);
 if(HIST.length>800)HIST.shift();
}
function lineChart(series,W,H){
 const all=series.reduce((a,s)=>a.concat(s.pts),[]);
 if(all.length<2)return '<div class="muted">(collecting samples…)</div>';
 const xs=all.map(p=>p[0]),ys=all.map(p=>p[1]);
 const xmin=Math.min.apply(null,xs),xmax=Math.max.apply(null,xs),ymax=Math.max.apply(null,ys.concat([1]));
 const pad=4,iw=W-2*pad,ih=H-2*pad;
 const sx=x=>pad+(xmax==xmin?0:(x-xmin)/(xmax-xmin)*iw);
 const sy=y=>pad+ih-y/ymax*ih;
 let svg='<svg width="100%" height="'+H+'" viewBox="0 0 '+W+' '+H+'" preserveAspectRatio="none" style="background:var(--secbg);border-radius:4px">';
 series.forEach(s=>{if(s.pts.length<2)return;const d=s.pts.map((p,i)=>(i?'L':'M')+sx(p[0]).toFixed(1)+' '+sy(p[1]).toFixed(1)).join(' ');
  svg+='<path d="'+d+'" fill="none" stroke="'+s.color+'" stroke-width="1.5"/>';});
 return svg+'</svg>';
}
function trends(){
 if(HIST.length<2)return chartCard('trends (over time)','<div class="muted">(collecting samples — charts appear as the build runs)</div>');
 const prog=lineChart([{pts:HIST.map(s=>[s.t,s.built]),color:'#22c55e'},{pts:HIST.map(s=>[s.t,s.failed]),color:'#ef4444'}],400,70);
 const disk=lineChart([{pts:HIST.map(s=>[s.t,s.disk]),color:'#06b6d4'}],400,70);
 // throughput: built per minute over the recent window
 const a=HIST[Math.max(0,HIST.length-40)],b=HIST[HIST.length-1],dt=(b.t-a.t)/60,tp=dt>0?((b.built-a.built)/dt).toFixed(1):'0.0';
 const last=HIST[HIST.length-1];
 return chartCard('build progress over time (green=built · red=failed)',prog+'<div class="legend">throughput ~'+tp+' built/min · '+last.built+' built · '+last.failed+' failed</div>')+
  chartCard('disk usage over time',disk+'<div class="legend">'+human(last.disk)+' total</div>');
}

// ---- W4: filter + export ----
let FILTER='';
function filterList(arr,keys){if(!FILTER)return arr;const q=FILTER.toLowerCase();
 return (arr||[]).filter(o=>keys.some(k=>(''+(o[k]==null?'':o[k])).toLowerCase().includes(q)))}
function setFilter(v){FILTER=v;render()}
function dl(name,type,data){const b=new Blob([data],{type:type}),u=URL.createObjectURL(b),a=document.createElement('a');a.href=u;a.download=name;a.click();setTimeout(()=>URL.revokeObjectURL(u),1000)}
function exportJSON(){dl('gflib-status.json','application/json',JSON.stringify(snap,null,1))}
function exportCSV(){const rows=[['slug','outcome','backend','compiler','bytes','compare','error']];
 (snap.built_recent||[]).forEach(b=>rows.push([b.slug,'built',b.backend||'',b.compiler_version||'',b.bytes||'',b.compare||'','']));
 (snap.failures_recent||[]).forEach(f=>rows.push([f.slug,'failed',f.backend||'',f.compiler_version||'','','',(f.error||'').replace(/[\r\n]+/g,' ')]));
 const csv=rows.map(r=>r.map(c=>'"'+(''+c).replace(/"/g,'""')+'"').join(',')).join('\n');
 dl('gflib-built-failed.csv','text/csv',csv)}

// ---- click-to-detail panel (mirrors the TUI build_detail; log tail via /api/log) ----
function findBy(arr,k,v){return (arr||[]).find(x=>x[k]==v)}
function openDetailIdx(n){const d=DET[n];if(d)openDetail(d[0],d[1])}
function openDetail(kind,id){
 let lines=[],slug=null,title='';
 if(kind=='cohort'){const c=findBy(snap.cohorts,'key',id);if(!c)return;title='Cohort: '+c.key;
  const ty=s=>c.families.filter(f=>f.status==s).length;
  lines=['families: '+c.count,'status: '+ty('built')+' built · '+ty('failed')+' failed · '+ty('building')+' building · '+(c.families.length-ty('built')-ty('failed')-ty('building'))+' queued','','family names (with build status):'];
  (c.families.length?c.families:[{name:'(none assigned yet)',status:''}]).forEach(f=>lines.push('  '+f.name+(f.status?' ['+f.status+']':'')));
  lines.push('','requirements:');(c.requirements?c.requirements.split('\n'):['(none — the base cohort has no requirements file)']).forEach(r=>lines.push('  '+r));
 } else if(kind=='built'){const b=findBy(snap.built_recent,'slug',id);if(!b)return;slug=id;title='Built: '+b.slug;
  lines=['backend: '+(b.backend||'?'),'output size: '+human(b.bytes),'vs shipped: '+(b.compare||'(not compared)'),'provenance: '+prov(b),'rebuild: gflib-build --only '+b.slug+' --rebuild --yes'];if(b.python_version)lines.splice(4,0,'python: '+b.python_version);
 } else if(kind=='failed'){const f=findBy(snap.failures_recent,'slug',id);slug=id;
  if(f){title='Failed: '+f.slug;lines=['provenance: '+prov(f),'rebuild: gflib-build --only '+f.slug+' --rebuild --yes','','error:','  '+(f.error||'')];if(f.rebuild_note)lines.unshift('⟳ REBUILD PENDING — '+f.rebuild_note,'');}
  else{// not in the recent window — fall back to the persistent failure history
   const hist=snap.failure_history||[];let h=null;for(let i=hist.length-1;i>=0;i--)if(hist[i].slug==id){h=hist[i];break;}
   if(h){title='Failed: '+h.slug+' (from failure history)';lines=['cause: '+h.cause,'provenance: '+prov(h),'rebuild: gflib-build --only '+h.slug+' --rebuild --yes','','error:','  '+(h.error||'')];}
   else{title='Failed: '+id;lines=['(no recorded error details for this family in the current snapshot)'];}}
 } else if(kind=='building'){const b=findBy(snap.building,'slug',id);if(!b)return;slug=id;title='Building: '+b.slug;
  lines=['worker: '+b.worker,'elapsed: '+hms(b.dur),'step: '+(b.note||b.backend||'(starting)')];
 } else if(kind=='queue'){const q=findBy(snap.queued_list,'slug',id);if(!q)return;title='Queued family: '+q.slug;
  const why={retry:'Re-attempt after a previous build FAILURE (its cause may now be fixable).',rebuild:'Rebuild of a family that already built successfully (forced by --rebuild or [R]).'}[q.kind]||'A fresh target — this family has never been built.';
  lines=['kind: '+q.kind,'',why];
 } else if(kind=='failcat'){const c=findBy(snap.fail_categories,'cat',id);if(!c)return;title='Failure cause: '+c.cat;
  lines=['families affected: '+c.count,'','affected families:'];(c.families&&c.families.length?c.families:['(none)']).forEach(s=>lines.push('  '+s));lines.push('','what to do:','  '+(c.hint||''));
 } else if(kind=='lintcat'){const c=(snap.lint_categories||[]).find(x=>x.severity+':'+x.tag==id);if(!c)return;title='lintian '+(c.severity=='E'?'error':'warning')+': '+c.tag;
  lines=['severity: '+(c.severity=='E'?'ERROR':'warning'),'packages affected: '+c.count,'lintian tag docs: https://lintian.debian.org/tags/'+c.tag,'','affected packages:'];(c.families&&c.families.length?c.families:['(none)']).forEach(s=>lines.push('  '+s));
 } else if(kind=='history'){const h=findBy(snap.failure_history,'slug',id);if(!h)return;slug=id;title='Failed (history): '+h.slug;
  lines=['cause: '+h.cause,'provenance: '+prov(h),'rebuild: gflib-build --only '+h.slug+' --rebuild --yes','','error:','  '+(h.error||'')];
 } else if(kind=='task'){const t=findBy(snap.tasks,'key',id);if(!t)return;title='Pipeline task: '+t.name;
  lines=['status: '+t.status];if(t.total)lines.push('progress: '+t.done+'/'+t.total);if(t.elapsed)lines.push('elapsed: '+hms(t.elapsed));if(t.detail)lines.push('detail: '+t.detail);
 } else if(kind=='package'){openPackage(id);return;
 } else if(kind=='tool'){const t=findBy(snap.tool_packages,'name',id);if(!t)return;title='Build-tool: '+t.name;
  const more=(t.family_list||[]).length<(t.families||0)?' (first '+(t.family_list||[]).length+')':'';
  lines=['language: '+(t.lang||'?')+'   kind: '+(t.kind||'?')+'   packaged: '+(t.packaged?'yes':'no'),'required by '+(t.families||0)+' families'+more,'','families:'];
  (t.family_list||[]).forEach(f=>lines.push('  '+f));
 } else if(kind=='fsfamily'){openFsFamily(id);return;
 } else return;
 showDetail(title,lines,slug);
}
// panel A detail: fetch a family's full fontspector result and list every check, worst-first
function openFsFamily(slug){
 const el=document.getElementById('detail');
 el.innerHTML='<div class="dhead">QA: '+E(slug)+'<span class="dclose" onclick="closeDetail()">✕ close</span></div><div id="fsfd" class="muted">loading…</div>';
 el.style.display='block';
 fetch('/api/fontspector?slug='+encodeURIComponent(slug)).then(r=>r.json()).then(d=>{
  const b=document.getElementById('fsfd');if(!b)return;
  if(!d.checks){b.textContent='(no QA result for this family — run gflib-build --fontspector)';return;}
  const ord={ERROR:6,FATAL:5,FAIL:4,WARN:3,INFO:2,PASS:1,SKIP:0};
  const checks=d.checks.slice().sort((a,b)=>(ord[b.status]||0)-(ord[a.status]||0));
  b.innerHTML='<div class="muted" style="margin:4px 0 8px">'+E(d.fontspector_version||'')+' · '+fsCount(d.counts||{})+'</div>'+
   checks.map(c=>'<div class="ln"><span class="'+fsCls(c.status)+'">'+(''+c.status).padEnd(6)+'</span> <span class="gr">'+E(c.id)+'</span>  <span class="muted">'+E(c.title)+'</span></div>').join('');
 }).catch(()=>{const b=document.getElementById('fsfd');if(b)b.textContent='(failed to load)'});
}
// package metadata panel: deb-build status + the actual debian/ file contents (via /api/debian)
function openPackage(slug){
 const b=findBy(snap.packages,'slug',slug)||{};
 const ds=b.deb_status||''; const st=ds||(b.packaged?'drafted':'draftable (built, not yet drafted)');
 const lint=b.deb_lint||''; const built=(ds=='built'||ds=='validated'||ds=='lint-clean');
 const hasReport=lint&&lint!='not run (lintian absent)'&&lint!='lintian failed to run';
 const acts=(built?'<a class="rb" href="/api/deb?slug='+encodeURIComponent(slug)+'" download title="download the built .deb">⬇ .deb</a>':'')+
   (hasReport?'<button class="rb '+(/error/.test(lint)?'r':'')+'" onclick="openLintian(\''+E(slug)+'\')" title="read the lintian report">▤ lintian report</button>':'');
 const lintTag=lint?' &nbsp;<span class="muted">lintian: '+E(lint)+'</span>':'';
 const el=document.getElementById('detail');
 el.innerHTML='<div class="dhead">Package: '+E(slug)+' &nbsp;<span class="muted">deb status: '+E(st)+'</span>'+lintTag+' &nbsp;'+acts+'<span class="dclose" onclick="closeDetail()">✕ close</span></div><pre class="dbody" id="pkgmeta">loading…</pre>';
 el.style.display='block';
 fetch('/api/debian?slug='+encodeURIComponent(slug)).then(r=>r.text()).then(t=>{const m=document.getElementById('pkgmeta');if(m)m.textContent=t}).catch(()=>{const m=document.getElementById('pkgmeta');if(m)m.textContent='(failed to load)'});
}
// lintian report overlay: fetch the saved report and colour E:/W:/I: lines
function openLintian(slug){
 const el=document.getElementById('detail');
 el.innerHTML='<div class="dhead">lintian — '+E(slug)+' &nbsp;<a class="rb" href="/api/deb?slug='+encodeURIComponent(slug)+'" download title="download the built .deb">⬇ .deb</a><span class="dclose" onclick="closeDetail()">✕ close</span></div><pre class="dbody" id="lintbody">loading…</pre>';
 el.style.display='block';
 fetch('/api/lintian?slug='+encodeURIComponent(slug)).then(r=>r.text()).then(t=>{const m=document.getElementById('lintbody');if(m)m.innerHTML=hlLint(t)}).catch(()=>{const m=document.getElementById('lintbody');if(m)m.textContent='(failed to load)'});
}
function hlLint(t){return t.split('\n').map(l=>{const c=l[0]=='E'?'r':l[0]=='W'?'y':(l[0]=='I'||l[0]=='P')?'c':'gr';return '<span class="'+c+'">'+E(l)+'</span>'}).join('\n')}
function showDetail(title,lines,slug){
 let h='<div class="dhead">'+E(title)+'<span class="dclose" onclick="closeDetail()">✕ close</span></div><pre class="dbody">'+lines.map(E).join('\n')+'</pre>';
 if(slug)h+='<div class="dlog"><div class="muted">log tail (last 200 lines):</div><pre id="dlogbody" class="dbody">loading…</pre></div>';
 const el=document.getElementById('detail');el.innerHTML=h;el.style.display='block';
 if(slug)fetch('/api/log?slug='+encodeURIComponent(slug)+'&n=200').then(r=>r.text()).then(t=>{const b=document.getElementById('dlogbody');if(b)b.innerHTML=hlLog(t)}).catch(()=>{});
}
function closeDetail(){const el=document.getElementById('detail');el.style.display='none';el.innerHTML=''}
addEventListener('keydown',e=>{if(e.key=='Escape')closeDetail()});

// ---- log syntax highlighting (per-line classification of gflib-build's build logs) ----
function logCls(l){
 if(/^\[\+/.test(l))return /FAIL/.test(l)?'r':(/\bok\b/.test(l)?'g':'c');     // [+ N.Ns] phase markers
 if(/^#|^=====|^\[\d+\/\d+\]/.test(l))return 'c';                              // meta / banners / [N/M]
 if(/^\s*File ".*", line \d+|^\s*[~^]+\s*$/.test(l))return 'muted';            // traceback frames / carets
 if(/Traceback|Command failed|^FAILED\b|^\s*\w*(Error|Exception):|\berror\[|\berror:|\bERROR\b|\bpanic/.test(l))return 'r';
 if(/\bWARNING\b|\bwarning:|\bWARN\b/.test(l))return 'y';
 if(/Successfully|\bPASS\b|:\s*ok\b|✓/.test(l))return 'g';
 if(/^INFO:|^DEBUG:|\bINFO\b/.test(l))return 'muted';
 return 'gr';
}
function hlLog(t){return t.split('\n').map(l=>'<span class="'+logCls(l)+'">'+E(l)+'</span>').join('\n')}

// ---- charts: hand-rolled, dependency-free (CSS-div bars + inline-SVG donuts/rings) ----
function chartCard(title,inner){return '<div class="chart"><div class="ctitle">'+E(title)+'</div>'+inner+'</div>'}
function barChart(items,unit){
 if(!items.length)return '<div class="muted">(no data yet)</div>';
 const max=Math.max.apply(null,items.map(i=>i.value).concat([1]));
 return '<div class="bars">'+items.map(i=>'<div class="brow"><span class="blabel" title="'+E(i.label)+'">'+E(i.label)+'</span>'+
  '<span class="btrack"><span class="bfill" style="width:'+(100*i.value/max).toFixed(1)+'%;background:'+i.color+'"></span></span>'+
  '<span class="bval">'+E(i.disp!=null?i.disp:i.value)+(unit||'')+'</span></div>').join('')+'</div>';
}
function donut(slices,cx){
 const r=cx-9,C=2*Math.PI*r,total=slices.reduce((a,s)=>a+(s.value||0),0)||1;let off=0,arcs='';
 slices.forEach(s=>{const len=C*(s.value||0)/total;if(len>0){
  arcs+='<circle cx="'+cx+'" cy="'+cx+'" r="'+r+'" fill="none" stroke="'+s.color+'" stroke-width="13" stroke-dasharray="'+len+' '+(C-len)+'" stroke-dashoffset="'+(-off)+'" transform="rotate(-90 '+cx+' '+cx+')"/>';off+=len;}});
 if(!arcs)arcs='<circle cx="'+cx+'" cy="'+cx+'" r="'+r+'" fill="none" stroke="#1e293b" stroke-width="13"/>';
 return '<svg width="'+(cx*2)+'" height="'+(cx*2)+'" viewBox="0 0 '+(cx*2)+' '+(cx*2)+'">'+arcs+'</svg>';
}
function ring(done,total,sub){
 const r=34,C=2*Math.PI*r,frac=total?done/total:0,len=C*frac;
 return '<svg width="88" height="88" viewBox="0 0 88 88">'+
  '<circle cx="44" cy="44" r="34" fill="none" stroke="#1e293b" stroke-width="10"/>'+
  (len>0?'<circle cx="44" cy="44" r="34" fill="none" stroke="#06b6d4" stroke-width="10" stroke-linecap="round" stroke-dasharray="'+len+' '+(C-len)+'" transform="rotate(-90 44 44)"/>':'')+
  '<text x="44" y="42" text-anchor="middle" class="rtxt" font-size="15" font-weight="600">'+Math.floor(100*frac)+'%</text>'+
  '<text x="44" y="58" text-anchor="middle" fill="#7c8aa0" font-size="9">'+E(sub||'')+'</text></svg>';
}
function legend(slices){return '<div class="legend">'+slices.filter(s=>(s.value||0)>0).map(s=>'<span><i style="background:'+s.color+'"></i>'+E(s.label)+' '+(s.value||0)+'</span>').join('')+'</div>'}

// ---- packaging tab: deb toolchain (left) + package-status pie (right), then the per-family list ----
// canonical package-status order / colour / tooltip for the pie + secondary bar (colours match the rows)
const PKG_STATES=[
 ['lint-clean','#22c55e','validated AND lintian clean (no errors or warnings)'],
 ['lint-warn','#fb923c','validated; lintian passed with NO errors, only warnings'],
 ['lintian-fail','#ef4444','validated by dpkg-deb but lintian reported errors'],
 ['validated','#0e9bbd','dpkg-deb ok (control parses, contains fonts); lintian has not run yet'],
 ['built','#06b6d4','.deb produced but did NOT pass dpkg-deb validation (control failed, or no fonts inside)'],
 ['drafted','#eab308','a debian/ tree is drafted on disk; the .deb is not built yet'],
 ['draftable','#64748b','built family, ready to draft a debian/ tree'],
 ['no-fonts','#7c6f9c','fonts were discarded — not packaged yet (re-run a build with .deb packaging on to keep the fonts)'],
 ['deb-failed','#b91c1c','dpkg-deb failed to build the .deb'],
];
function pkgStatusKey(b){const ds=b.deb_status||'';
 if(ds=='lint-clean'||ds=='lint-warn'||ds=='lintian-fail'||ds=='validated'||ds=='built')return ds;
 if(ds=='no-fonts')return 'no-fonts';
 if(ds=='failed')return 'deb-failed';
 return b.packaged?'drafted':'draftable';}
function pkgCounts(){const cnt={};(snap.packages||[]).forEach(b=>{const k=pkgStatusKey(b);cnt[k]=(cnt[k]||0)+1});return cnt;}
function packagingView(){
 const secs=sections('packaging');
 // deb toolchain on the LEFT half; packaging-queue panel + package-status pie on the RIGHT half;
 // lintian-category breakdown and the per-family list below (full width)
 const left=secs[0]?renderSec(secs[0]):'';
 return '<div class="panes"><div>'+left+'</div><div>'+packagingQueue()+packagingPie()+'</div></div>'+
   lintCatChart()+lintCatSection()+(secs[1]?renderSec(secs[1]):'');
}
// the .deb packaging/lint queue — mirrors the main build queue (live progress + current activity + backlog)
function packagingQueue(){
 const now=snap.pkg_now||'',lt=snap.lint_total||0,ld=snap.lint_done||0,lp=snap.lint_pending||0,pp=snap.pkg_pending||0;
 const pct=lt?Math.floor(100*ld/lt):0,gw=lt?100*ld/lt:0;
 // packaging stage: every built family is a .deb candidate; packaged = built families minus the backlog
 const bt=(snap.counts&&snap.counts.built)||0,pd=Math.max(0,bt-pp),ppct=bt?Math.floor(100*pd/bt):0,pgw=bt?100*pd/bt:0;
 const lintAvail=(snap.deb_tools||[]).some(t=>t.name=='lintian'&&t.present);
 const act=now?('▶ '+E(now)):(snap.paused?'paused (the global pause also halts packaging/linting)':'idle — nothing to package or lint');
 const repkg='<button class="tbtn" title="rebuild EVERY .deb from the existing built fonts (no font rebuild) — applies packaging fixes (copyright/changelog) and re-lints each" onclick="if(confirm(\'Rebuild all .deb packages from the existing built fonts? This applies packaging fixes and re-lints every package — it can take a while.\'))ctl({repackage_all:true})">↻ repackage all</button>';
 const bar=(w,col,lbl)=>'<div class="barwrap" style="height:14px"><div class="seg" style="width:'+w+'%;background:'+col+'"></div><div class="seg" style="width:'+(100-w)+'%;background:#334155"></div><div class="barlbl">'+lbl+'</div></div>';
 return '<div class="chart"><div class="ctitle">packaging queue</div>'+
  '<div class="'+(now?'y':'muted')+'" style="margin:2px 0 5px">'+act+'</div>'+
  bar(pgw,'#06b6d4','packaged '+pd+' / '+bt+' ('+ppct+'%)')+   // stage 1: build the .deb
  bar(gw,'#22c55e','lintian '+ld+' / '+lt+' ('+pct+'%)')+      // stage 2: lint the .deb
  '<div class="legend"><span title="built families whose .deb is not built yet">to package: '+pp+'</span><span title="packages with a .deb that lintian has not run on yet">to lint: '+lp+'</span>'+
   (lintAvail?'':'<span class="r" title="install lintian via the deb toolchain panel to drain the lint queue">⚠ lintian not installed — lint queue stalled</span>')+'</div>'+
  '<div style="margin-top:6px">'+repkg+'</div></div>';
}
// lintian findings grouped by tag (the packaging analogue of the build "Failures by cause" view)
function lintcatRow(c){const isE=c.severity=='E';
 return {segs:[[Rp(c.count,4)+'  ',isE?'r':'o'],[(isE?'E ':'W ')+L(c.tag,38),isE?'r':'o'],[' '+c.count+' '+(c.count==1?'package':'packages'),'muted']],det:['lintcat',c.severity+':'+c.tag]}}
function lintCatChart(){const cats=(snap.lint_categories||[]).slice(0,10).map(c=>({label:(c.severity=='E'?'E ':'W ')+c.tag,value:c.count,color:c.severity=='E'?'#ef4444':'#fb923c'}));
 return cats.length?'<div class="chartrow">'+chartCard('lintian findings by category (top 10 · red=error · amber=warning)',barChart(cats))+'</div>':'';}
function lintCatSection(){const cats=snap.lint_categories||[];
 if(!cats.length)return '';
 return renderSec({title:'Lintian findings by category  (E = error · W = warning · click a row for the affected packages)',rows:cats.map(lintcatRow)});}
function packagingPie(){
 const pk=snap.packages||[],cnt=pkgCounts(),total=pk.length||1;
 const slices=PKG_STATES.filter(s=>cnt[s[0]]).map(s=>({label:s[0],value:cnt[s[0]],color:s[1],tip:s[2]}));
 if(!slices.length)return chartCard('package status','<div class="muted">(no packages yet)</div>');
 return chartCard('package status — '+pk.length+' families','<div class="dwrap">'+donutT(slices,64)+pieLegend(slices,total)+'</div>');
}
// donut with a <title> tooltip per slice (label · count · % · explanation)
function donutT(slices,cx){
 const r=cx-9,C=2*Math.PI*r,total=slices.reduce((a,s)=>a+(s.value||0),0)||1;let off=0,arcs='';
 slices.forEach(s=>{const len=C*(s.value||0)/total;if(len>0){const pct=Math.round(100*s.value/total);
  arcs+='<circle cx="'+cx+'" cy="'+cx+'" r="'+r+'" fill="none" stroke="'+s.color+'" stroke-width="14" stroke-dasharray="'+len+' '+(C-len)+'" stroke-dashoffset="'+(-off)+'" transform="rotate(-90 '+cx+' '+cx+')"><title>'+E(s.label+': '+s.value+' ('+pct+'%) — '+(s.tip||''))+'</title></circle>';off+=len;}});
 if(!arcs)arcs='<circle cx="'+cx+'" cy="'+cx+'" r="'+r+'" fill="none" stroke="#1e293b" stroke-width="14"/>';
 return '<svg width="'+(cx*2)+'" height="'+(cx*2)+'" viewBox="0 0 '+(cx*2)+' '+(cx*2)+'">'+arcs+'</svg>';
}
function pieLegend(slices,total){return '<div class="legend">'+slices.map(s=>{const pct=Math.round(100*s.value/total);
 return '<span title="'+E(s.tip||'')+'"><i style="background:'+s.color+'"></i>'+E(s.label)+' '+s.value+' ('+pct+'%)</span>';}).join('')+'</div>'}
// secondary bar under the main one: deb-packaging status of the built families, scaled to the WIDTH of
// the green "built" segment (gw% of the container) so each stage lines up beneath the built portion.
function packagingBar(gw){
 const pk=snap.packages||[];if(!pk.length||gw<=0)return '';
 const cnt=pkgCounts(),total=pk.length;
 const inner=PKG_STATES.filter(s=>cnt[s[0]]).map(s=>{const n=cnt[s[0]],w=100*n/total,pct=Math.round(w);
  // each portion is labelled with its stage name + count (clipped by overflow when the segment is too narrow)
  return '<div class="seg" style="width:'+w+'%;background:'+s[1]+'" title="'+E(s[0]+': '+n+' ('+pct+'% of built) — '+s[2])+'"><span class="sl">'+E(s[0])+' '+n+'</span></div>';}).join('');
 return '<div class="barwrap pkgbar" style="width:'+gw+'%" title="packaging status of the '+total+' built families">'+inner+'</div>';
}

function charts(t){
 const c=snap.counts||{};
 if(t=='overview'){
  const sl=[{label:'built',value:c.built||0,color:'#22c55e'},{label:'failed',value:c.failed||0,color:'#ef4444'},{label:'building',value:c.building||0,color:'#06b6d4'},{label:'queued',value:c.queued||0,color:'#eab308'},{label:'skipped',value:c.skipped||0,color:'#475569'}];
  const fc=(snap.fail_categories||[]).slice().sort((a,b)=>b.count-a.count).slice(0,6).map(x=>({label:x.cat,value:x.count,color:'#ef4444'}));
  return '<div class="chartrow">'+chartCard('outcome','<div class="dwrap">'+donut(sl,52)+legend(sl)+'</div>')+chartCard('top failure causes',barChart(fc))+'</div>'+
   '<div class="chartrow">'+trends()+'</div>';
 }
 if(t=='failures'){
  const fc=(snap.fail_categories||[]).slice().sort((a,b)=>b.count-a.count).map(x=>({label:x.cat,value:x.count,color:'#ef4444'}));
  return fc.length?'<div class="chartrow">'+chartCard('failures by cause',barChart(fc))+'</div>':'';
 }
 if(t=='stats'){
  const ops=Object.entries(snap.op_stats||{}).sort((a,b)=>(b[1].total||0)-(a[1].total||0)).slice(0,8).map(e=>({label:e[0],value:e[1].total||0,color:'#06b6d4',disp:hms(e[1].total||0)}));
  const m=snap.migration||{};
  const sl=[{label:'fontc',value:m.fontc||0,color:'#22c55e'},{label:'fontmake-fallback',value:m.fontmake_fallback||0,color:'#eab308'},{label:'fontmake-only',value:m.fontmake_only||0,color:'#ef4444'},{label:'both',value:(m.both_identical||0)+(m.both_differ||0),color:'#06b6d4'}];
  return '<div class="chartrow">'+chartCard('operation timing (bottlenecks)',barChart(ops))+chartCard('backend mix','<div class="dwrap">'+donut(sl,52)+legend(sl)+'</div>')+'</div>';
 }
 if(t=='cohorts'){
  const co=(snap.cohorts||[]).slice().sort((a,b)=>b.count-a.count).slice(0,12).map(x=>({label:x.key,value:x.count,color:x.cached?'#22c55e':'#475569'}));
  const leg='<div class="legend" style="margin:6px 2px">family name colour: <span><i style="background:#86efac"></i>built</span><span><i style="background:#fca5a5"></i>failed</span><span><i style="background:#fde68a"></i>building</span><span><i style="background:#7c8aa0"></i>not yet attempted</span> · click a cohort row for details</div>';
  return (co.length?'<div class="chartrow">'+chartCard('cohort sizes (green = venv cached on disk)',barChart(co))+'</div>':'')+leg;
 }
 if(t=='archive'){
  const a=snap.archive||{},mir=a.total||0,pend=a.pending_total||0;
  return '<div class="chartrow">'+chartCard('archive mirroring','<div class="dwrap">'+ring(mir,mir+pend,mir+' / '+(mir+pend))+
   '<div class="legend"><span><i style="background:#06b6d4"></i>mirrored '+mir+'</span><span><i style="background:#1e293b"></i>queued '+pend+'</span></div></div>')+'</div>';
 }
 return '';
}

addEventListener('hashchange',()=>{const t=location.hash.slice(1);if(TABS.includes(t)){tab=t;render()}});
if(TABS.includes(location.hash.slice(1)))tab=location.hash.slice(1);

// ---- W5: automatic adaptive polling. There is no user knob: the page refreshes fast enough to feel
//      instantaneous, self-throttles so it can never flood the daemon (a new request starts only after
//      the previous one returns — see poll()), eases off when nothing is changing, snaps back to fast
//      on any change or user action, and parks entirely while the tab is hidden. ----
const POLL_FAST=500, POLL_SLOW=2500, POLL_RAMP=4; // ms; POLL_RAMP = unchanged polls tolerated before easing off
let pollTimer=null, polling=false, lastSig='', idleStreak=0, lastDone=false;
function schedulePoll(ms){if(pollTimer)clearTimeout(pollTimer);if(document.hidden)return;
 pollTimer=setTimeout(poll, ms!=null?ms:(idleStreak>POLL_RAMP?POLL_SLOW:POLL_FAST))}
function bump(){idleStreak=0;schedulePoll(POLL_FAST)} // user acted → return to fast cadence so the change shows promptly
document.addEventListener('visibilitychange',()=>{if(document.hidden){if(pollTimer){clearTimeout(pollTimer);pollTimer=null}}else{idleStreak=0;poll()}});
function setTheme(t){document.body.dataset.theme=t;localStorage.setItem('gf_theme',t)}
function toggleTheme(){setTheme(document.body.dataset.theme=='light'?'dark':'light')}
function askNotify(){if(window.Notification)Notification.requestPermission().then(()=>render())}
function checkNotify(){const done=snap.done&&(snap.total||0)>0;
 if(done&&!lastDone&&window.Notification&&Notification.permission=='granted'){
  const c=snap.counts||{};new Notification('gflib-build — build complete',{body:(c.built||0)+' built · '+(c.failed||0)+' failed · '+(c.skipped||0)+' skipped'});}
 lastDone=done;}
setTheme(localStorage.getItem('gf_theme')||'dark');
poll(); // schedules its own next run; the adaptive loop takes over from here
</script></body></html>"###;
