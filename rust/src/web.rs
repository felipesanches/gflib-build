//! Web dashboard (`--ui web`) — a dependency-free HTTP/1.1 server (std `TcpListener`, one thread per
//! connection) that mirrors the TUI: it serves the snapshot at `/api/status` and routes live controls
//! (jobs / percent / pause / retry) to control.json via `POST /api/control` — the same channel the
//! curses monitor uses. The browser page polls `/api/status` every 1.5 s and renders every tab.

use crate::model::ControlSet;
use crate::monitor::Source;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn run(source: Arc<dyn Source>, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("gflib-build web dashboard: http://127.0.0.1:{}/", port);
    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let src = Arc::clone(&source);
            std::thread::spawn(move || {
                let _ = handle(stream, src);
            });
        }
    }
    Ok(())
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

const PAGE: &str = r###"<!doctype html><html><head><meta charset="utf-8">
<title>gflib-build dashboard (Rust)</title>
<style>
 body{background:#0b0e14;color:#cbd5e1;font:13px/1.5 ui-monospace,Menlo,Consolas,monospace;margin:0;padding:12px}
 .t{font-size:16px;color:#fff;font-weight:600}
 .muted{color:#7c8aa0}.c{color:#67e8f9}.g{color:#86efac}.r{color:#fca5a5}.y{color:#fde68a}
 .bar{background:#1e293b;border-radius:4px;height:14px;overflow:hidden;margin:6px 0}
 .fill{background:#22c55e;height:100%}
 .tabs{margin:10px 0}.tab{display:inline-block;padding:3px 10px;cursor:pointer;border-radius:4px;color:#9fb0c5}
 .tab.on{background:#1e293b;color:#fff}
 table{border-collapse:collapse;width:100%}td{padding:2px 8px;border-bottom:1px solid #1e293b;vertical-align:top}
 .card{background:#11161f;border:1px solid #1e293b;border-radius:6px;padding:10px;margin:8px 0}
 .s{color:#e2e8f0}.meta{color:#7c8aa0;float:right}
 button{background:#1e293b;color:#cbd5e1;border:1px solid #334155;border-radius:4px;padding:3px 8px;cursor:pointer}
</style></head><body>
<div id="hdr"></div>
<div class="bar"><div id="fill" class="fill" style="width:0%"></div></div>
<div class="tabs" id="tabs"></div>
<div class="muted" style="margin-bottom:6px">
 <button onclick="ctl({paused:true})">pause</button>
 <button onclick="ctl({paused:false})">resume</button>
 <button onclick="ctl({jobs:(snap.jobs||1)+1})">jobs+</button>
 <button onclick="ctl({jobs:Math.max(1,(snap.jobs||1)-1)})">jobs-</button>
 <span class="muted"> &nbsp; live polling every 1.5s · controls go to control.json</span>
</div>
<div id="body"></div>
<script>
let snap={}, tab='overview';
const TABS=['overview','queue','cohorts','built','failures','stats','archive','config'];
function human(n){n=n||0;const u=['B','KiB','MiB','GiB','TiB'];let i=0;while(n>=1024&&i<u.length-1){n/=1024;i++}return (i?n.toFixed(1):n)+u[i]}
function hms(s){s=Math.max(0,s|0);return [s/3600|0,(s%3600)/60|0,s%60].map(x=>String(x).padStart(2,'0')).join(':')}
function E(s){return (s==null?'':''+s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
function prov(x){const c=x.compiler_version||x.backend||'';return c+(x.builder_version?' · '+x.builder_version:'')}
function ctl(set){fetch('/api/control',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({set:set})})}
function setTab(t){tab=t;render()}
async function poll(){try{snap=await (await fetch('/api/status')).json()}catch(e){}render();}
function render(){
 const c=snap.counts||{},processed=(c.built||0)+(c.failed||0)+(c.skipped||0),inscope=processed+(c.queued||0)+(c.building||0);
 const pct=inscope?Math.round(processed*100/inscope):0;
 const bld=snap.disk_build_total||0,arc=snap.disk_archive_total||0;
 const disk=snap.disk_archive_nested?('disk used '+human(bld)+' (build + nested archive, all included)'):('disk used '+human(bld+arc)+' (build '+human(bld)+' + archive '+human(arc)+')');
 document.getElementById('hdr').innerHTML='<div class="t">Google Fonts library build — Rust port'+(snap.paused?' <span class="y">[PAUSED]</span>':'')+
   '<span class="meta">elapsed '+hms(snap.elapsed)+'</span></div><div class="muted">'+disk+' · free '+human(snap.disk_free)+
   ' · jobs '+(snap.jobs||0)+' · fontc '+((snap.backends||{}).fontc||0)+'/fontmake '+((snap.backends||{}).fontmake||0)+
   ' · Phase '+E(snap.phase)+': built '+(c.built||0)+' failed '+(c.failed||0)+' building '+(c.building||0)+' queued '+(c.queued||0)+'</div>';
 document.getElementById('fill').style.width=pct+'%';
 document.getElementById('tabs').innerHTML=TABS.map(t=>'<span class="tab'+(t==tab?' on':'')+'" onclick="setTab(\''+t+'\')">'+t+'</span>').join('');
 document.getElementById('body').innerHTML=views[tab]?views[tab]():'';
}
function rows(arr,fn){return '<table>'+(arr||[]).map(fn).join('')+'</table>'}
const views={
 overview:()=>card('Now building ('+(snap.building||[]).length+')',rows(snap.building,b=>'<tr><td class="s">w'+b.worker+' '+E(b.slug)+'</td><td class="meta">'+hms(b.dur)+' '+E(b.note)+'</td></tr>'))+
   card('Recent failures ('+(snap.failures_recent||[]).length+')',rows(snap.failures_recent,f=>'<tr><td class="r">'+E(f.slug)+'</td><td>'+E(f.error)+'</td></tr>')),
 queue:()=>card('Queue ('+(snap.queued_list||[]).length+')',rows(snap.queued_list,q=>'<tr><td class="s">'+E(q.slug)+'</td><td class="meta">'+E(q.kind)+'</td></tr>')),
 cohorts:()=>card('Cohorts ('+(snap.cohorts||[]).length+')',rows(snap.cohorts,c=>'<tr><td class="s">'+E(c.key)+'</td><td class="meta">'+c.count+'</td></tr>')),
 built:()=>card('Built ('+(snap.built_recent||[]).length+')',rows(snap.built_recent,b=>'<tr><td class="g s">'+E(b.slug)+'</td><td class="c">'+E(prov(b))+'</td><td class="meta">'+human(b.bytes)+' '+E(b.compare||'')+'</td></tr>')),
 failures:()=>card('Failures ('+(snap.failures_recent||[]).length+')',rows(snap.failures_recent,f=>'<tr><td class="r s">'+E(f.slug)+'</td><td class="c">'+E(prov(f))+'</td><td>'+E(f.error)+'</td></tr>')),
 stats:()=>{let h=card('Migration','<table>'+Object.keys(snap.migration||{}).map(k=>'<tr><td>'+E(k)+'</td><td>'+snap.migration[k]+'</td></tr>').join('')+'</table>');
   const tl=snap.tooling||{},bl=snap.builders||{};
   if(Object.keys(tl).length)h+=card('Compilers in use','<table>'+Object.keys(tl).map(k=>'<tr><td>'+E(k)+'</td><td>'+E(tl[k])+'</td></tr>').join('')+'</table>');
   if(Object.keys(bl).length)h+=card('Builders in use','<table>'+Object.keys(bl).map(k=>'<tr><td>'+E(k)+'</td><td>'+E(bl[k])+'</td></tr>').join('')+'</table>');
   h+=card('Failure causes',rows(snap.fail_categories,f=>'<tr><td class="meta">'+f.count+'</td><td>'+E(f.cat)+'</td><td class="muted">'+E(f.hint)+'</td></tr>'));return h},
 archive:()=>card((snap.archive||{}).total+' repos mirrored on disk',rows((snap.archive||{}).recent,r=>'<tr><td class="'+(r.status=='failed'?'r':'g')+'">'+E(r.repo)+'</td><td class="muted">'+E(r.reason)+'</td></tr>')),
 config:()=>card('Configuration','<table>'+Object.keys(snap.config||{}).map(k=>'<tr><td>'+E(k)+'</td><td>'+E(''+snap.config[k])+'</td></tr>').join('')+'</table>')+
   card('Recent live changes',rows((snap.control_log||[]).slice(-8).reverse(),l=>'<tr><td>'+E(l)+'</td></tr>')),
};
function card(t,b){return '<div class="card"><div class="c" style="margin-bottom:4px">'+E(t)+'</div>'+b+'</div>'}
poll();setInterval(poll,1500);
</script></body></html>"###;
