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

// The single-page dashboard. Dependency-free: vanilla JS, no CDN/npm. Structurally parallel to the
// curses TUI — same tab order, same sections, same colours/formats. (W1 of WEB_UI_PLAN.md.)
const PAGE: &str = r###"<!doctype html><html><head><meta charset="utf-8">
<title>gflib-build dashboard</title>
<style>
 :root{--g:#86efac;--r:#fca5a5;--c:#67e8f9;--y:#fde68a;--muted:#7c8aa0;--dr:#c77d7d;--w:#fff;--gr:#cbd5e1;--bg:#0b0e14;--panel:#11161f;--line:#1e293b}
 body{background:var(--bg);color:var(--gr);font:13px/1.5 ui-monospace,Menlo,Consolas,monospace;margin:0;padding:10px 12px}
 .g{color:var(--g)}.r{color:var(--r)}.c{color:var(--c)}.y{color:var(--y)}.muted{color:var(--muted)}.dr{color:var(--dr)}.w{color:var(--w)}.gr{color:var(--gr)}.b{font-weight:600}
 .t{font-size:15px;color:#fff;font-weight:600}
 .sub{color:var(--c)}
 .right{float:right}
 /* segmented progress bar */
 .barwrap{position:relative;height:18px;background:var(--line);border-radius:4px;overflow:hidden;margin:6px 0;display:flex}
 .seg{height:100%}.seg.bg{background:#22c55e}.seg.rg{background:#ef4444}.seg.dg{background:#334155}.seg.cg{background:#06b6d4}
 .barlbl{position:absolute;left:0;right:0;top:0;line-height:18px;text-align:center;color:#fff;font-weight:600;font-size:11px;text-shadow:0 0 3px #000}
 .phase{margin:4px 0 0}
 .skip{color:var(--y);float:right}
 /* tabs */
 .tabs{margin:8px 0 4px;border-bottom:1px solid var(--line);padding-bottom:4px}
 .tab{display:inline-block;padding:3px 11px;cursor:pointer;border-radius:4px 4px 0 0;color:var(--muted)}
 .tab.on{background:var(--line);color:#fff}
 .tabhint{float:right;color:var(--muted);font-size:11px;padding-top:4px}
 /* controls */
 .ctl{margin:4px 0 8px;color:var(--muted)}
 button{background:var(--line);color:var(--gr);border:1px solid #334155;border-radius:4px;padding:2px 9px;cursor:pointer;font:inherit}
 button:disabled{opacity:.4;cursor:default}
 .rb{visibility:hidden;margin-left:8px;padding:0 6px;font-size:11px}
 .ln:hover .rb{visibility:visible}
 /* sections + rows */
 .sec{background:#0e1420;border-left:3px solid #334155;color:#fff;font-weight:600;padding:3px 8px;margin:10px 0 2px;border-radius:0 4px 4px 0}
 .ln{white-space:pre;padding:1px 8px;border-radius:3px}
 .ln:hover{background:#0e1420}
 .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(26ch,1fr));gap:0 8px;padding:2px 8px}
 .pin{background:#1a1505;border:1px solid #3b2f08;border-radius:6px;padding:4px 0;margin:6px 0}
 .pin .sec{background:none;border:none;color:var(--y);margin:2px 0}
 .cfg td{padding:1px 10px 1px 8px;white-space:pre}
 /* charts (hand-rolled, dependency-free: CSS-div bars + inline-SVG donuts/rings) */
 .chartrow{display:flex;flex-wrap:wrap;gap:12px;margin:8px 0}
 .chart{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:10px 12px;flex:1;min-width:250px}
 .ctitle{color:#fff;font-weight:600;margin-bottom:8px;font-size:12px}
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
</style></head><body>
<div id="hdr"></div>
<div id="bar"></div>
<div class="tabs" id="tabs"></div>
<div class="ctl" id="ctl"></div>
<div id="pin"></div>
<div id="body"></div>
<script>
let snap={}, tab='overview';
// tab order MUST match the TUI's VIEWS
const TABS=['config','overview','queue','cohorts','archive','built','failures','stats'];
const TASK_MARK={done:'✅',failed:'❌',running:'🔄',skipped:'➖',pending:'⏳'};
const TASK_CLS={done:'g',failed:'r',running:'y',skipped:'muted',pending:'gr'};
// the full CONFIG_SCHEMA (display order), mirroring the TUI
const SCHEMA=[
 {k:'source',l:'worklist source',t:'choice',live:false},
 {k:'google_fonts',l:'google/fonts clone',t:'path',live:false},
 {k:'archive',l:'repo archive',t:'path',live:false},
 {k:'build_dir',l:'build output dir',t:'path',live:false},
 {k:'backend',l:'build backend',t:'choice',live:true},
 {k:'fontc_bin',l:'fontc binary',t:'path',live:false},
 {k:'build_fontc',l:'build fontc from source (if none)',t:'bool',live:false},
 {k:'jobs',l:'parallel jobs',t:'step',live:true},
 {k:'percent',l:'percent of library',t:'step',live:true},
 {k:'timeout',l:'per-build timeout (0=off)',t:'step',live:true},
 {k:'populate_archive',l:'populate archive (fetch repos)',t:'bool',live:true},
 {k:'manage_venvs',l:'cohort venvs',t:'bool',live:false},
 {k:'retry_failed',l:'retry ALL failed (incl. genuine errors)',t:'bool',live:false},
 {k:'compare',l:'compare to shipped',t:'bool',live:true},
];
function human(n){n=n||0;const u=['B','KiB','MiB','GiB','TiB'];let i=0;while(n>=1024&&i<u.length-1){n/=1024;i++}return (i?n.toFixed(1):n)+u[i]}
function hms(s){s=Math.max(0,s|0);return [s/3600|0,(s%3600)/60|0,s%60].map(x=>String(x).padStart(2,'0')).join(':')}
function E(s){return (s==null?'':''+s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
function L(s,n){s=(s==null?'':''+s);return s.length>n?s.slice(0,n):s.padEnd(n)}
function Rp(s,n){return (''+s).padStart(n)}
function trunc(s,n){s=(s==null?'':''+s);return s.length>n?s.slice(0,n-1)+'…':s}
function prov(x){const c=x.compiler_version||x.backend||'';return c+(x.builder_version?' · '+x.builder_version:'')}
function ctl(set){fetch('/api/control',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({set:set})})}
function setTab(t){tab=t;location.hash=t;render()}
async function poll(){try{snap=await (await fetch('/api/status')).json()}catch(e){}render()}

// --- a row = array of [text, colour-class] segments; rt = optional retry slug ---
function R(segs,rt){
 let h='<div class="ln">'+segs.map(s=>'<span class="'+s[1]+'">'+E(s[0])+'</span>').join('');
 if(rt) h+='<button class="rb" onclick="ctl({retry:[\''+E(rt)+'\']})" title="retry this family">↻ retry</button>';
 return h+'</div>';
}
function secHdr(title,n){return '<div class="sec">'+E(title)+' ('+n+')</div>'}
function renderSec(s){return secHdr(s.title,s.rows.length)+(s.rows.length?s.rows.map(r=>R(r.segs,r.rt)).join(''):'<div class="ln muted">(none)</div>')}

// --- per-row builders (formats + colours match the TUI exactly) ---
function taskRow(t){const m=TASK_MARK[t.status]||'?',cl=TASK_CLS[t.status]||'gr';
 const prog=t.total?(t.done+'/'+t.total):'',el=t.elapsed?hms(t.elapsed):'';
 return {segs:[[m+' '+L(t.name,26)+' '+L(prog,11)+Rp(el,8)+'  '+(t.detail||''),cl]]}}
function failRow(f){return {segs:[[L(f.slug,34)+' ','r'],[f.error||'','dr']],rt:f.slug}}
function qRow(q){const kc={retry:'y',rebuild:'c'}[q.kind]||'g';return {segs:[['  '+L(q.kind,8)+' ',kc],[q.slug||'','gr']],rt:q.slug}}
function cohortRow(c){const segs=[[c.cached?'● ':'○ ',c.cached?'g':'muted'],[Rp(c.count,4)+'  '+L(c.key,14)+' ',c.key=='base'?'w':'c']];
 const f=c.families||[];if(!f.length)segs.push(['(no families yet)','g']);else f.forEach((n,i)=>{if(i)segs.push([' | ','c']);segs.push([n,'g'])});return {segs}}
function builtRow(b){const comp=b.compiler_version||b.backend||'';
 return {segs:[[L(b.slug,32)+' ','g'],[L(comp,26)+' ','c'],[Rp(human(b.bytes),9)+'  '+(b.compare||''),'gr']],rt:b.slug}}
function failcatRow(c){return {segs:[[Rp(c.count,4)+'  ','w'],[L(c.cat,24),'c'],[' '+(c.hint||''),'muted']]}}
function histRow(h){return {segs:[[L(h.cause,20)+' ','y'],[h.slug||'','gr']],rt:h.slug}}
function phaseRow(kv){return {segs:[[L(kv[0],12)+' '+hms(kv[1]),'gr']]}}
function opRow(kv){const s=kv[1];return {segs:[[L(kv[0],10)+' total '+Rp((s.total||0).toFixed(1),9)+'  n '+Rp(s.count||0,5)+'  mean '+Rp((s.mean||0).toFixed(2),7)+'  max '+Rp((s.max||0).toFixed(1),7),'c']]}}
function buildingRow(b){const note=b.note||b.backend||'';return {segs:[['w'+Rp(b.worker,2)+' '+L(b.slug,34)+' '+Rp(hms(b.dur),8)+'  '+note,'y']]}}

function sections(t){
 if(t=='overview')return [{title:'Pipeline',rows:(snap.tasks||[]).map(taskRow)},{title:'Recent failures',rows:(snap.failures_recent||[]).map(failRow)}];
 if(t=='queue')return [{title:'Queued — priority order (variable + larger families first)',rows:(snap.queued_list||[]).map(qRow)}];
 if(t=='cohorts')return [{title:'Dependency cohorts  (● = venv cached on disk, reused next run)',rows:(snap.cohorts||[]).map(cohortRow)}];
 if(t=='built')return [{title:'Built — successes  (slug · compiler+version · size · vs-shipped)',rows:(snap.built_recent||[]).map(builtRow)}];
 if(t=='failures'){const s=[];if((snap.fail_categories||[]).length)s.push({title:'Failures by cause',rows:snap.fail_categories.map(failcatRow)});
  s.push({title:'Failures — newest first (current)',rows:(snap.failures_recent||[]).map(failRow)});
  if((snap.failure_history||[]).length)s.push({title:'Failure history (persistent — survives restarts & re-attempts)',rows:snap.failure_history.map(histRow)});return s}
 if(t=='stats'){const ph=Object.entries(snap.phase_durations||{}).sort((a,b)=>b[1]-a[1]);
  const ops=Object.entries(snap.op_stats||{}).sort((a,b)=>(b[1].total||0)-(a[1].total||0));
  return [{title:'Phase timing',rows:ph.map(phaseRow)},{title:'Operation timing',rows:ops.map(opRow)}]}
 return [];
}

function statsPrefix(){const m=snap.migration||{};
 let line='fontc '+(m.fontc||0)+'   fontmake-fallback(blockers) '+(m.fontmake_fallback||0)+'   fontmake-only '+(m.fontmake_only||0);
 if((m.both_identical||0)||(m.both_differ||0))line+='   both id '+(m.both_identical||0)+'/diff '+(m.both_differ||0);
 let h='<div class="sec">fontc migration</div><div class="ln g">'+E(line)+'</div>';
 const tl=snap.tooling||{},bl=snap.builders||{};
 if(Object.keys(tl).length)h+='<div class="ln c">compilers in use:  '+Object.entries(tl).map(e=>E(e[0]+' → '+e[1])).join('   ')+'</div>';
 if(Object.keys(bl).length)h+='<div class="ln c">builders in use:   '+Object.entries(bl).map(e=>E(e[0]+' → '+e[1])).join('   ')+'</div>';
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

function showIf(k,cf){const s=x=>(cf[x]==null?'':''+cf[x]);
 if(k=='google_fonts')return s('source')=='metadata';
 if(k=='fontc_bin')return s('backend')!='fontmake';
 if(k=='build_fontc')return s('backend')!='fontmake'&&!s('fontc_bin');
 if(k=='compare')return s('source')=='metadata';
 return true}
const CHOICES={source:['metadata','archive'],backend:['auto','fontc','fontmake','both']};
function cfgView(){const cf=snap.config||{};
 let h='<div class="sec">Configuration — edit settings (live where possible)</div><table class="cfg">';
 SCHEMA.filter(f=>showIf(f.k,cf)).forEach(f=>{
  let v=cf[f.k],val;
  if(f.t=='bool')val=v?'[x] yes':'[ ] no';
  else if(f.t=='choice'){const ch=CHOICES[f.k]||[];val='‹ '+(ch.includes(v)?v:(ch[0]||''))+' ›';}
  else val=(v==null?(f.k=='timeout'?'0':''):''+v);
  // read-only display: non-live fields are tagged (restart: C); live fields show no tag (no edits yet)
  const tag=(!f.live)?'<span class="muted">  (restart: C)</span>':'';
  h+='<tr><td class="w">'+E(f.l)+'</td><td class="'+(f.live?'y':'muted')+'">'+E(val)+tag+'</td></tr>';
 });
 h+='</table>';
 const dr=snap.dep_relaxations||[];
 if(dr.length)h+='<div class="sec">auto-fixed dependencies (no manual pinning needed)</div>'+dr.map(l=>'<div class="ln y">'+E(l)+'</div>').join('');
 const cl=snap.control_log||[];
 if(cl.length)h+='<div class="sec">applied live changes</div>'+cl.slice().reverse().map(l=>'<div class="ln g">'+E(l)+'</div>').join('');
 return h;
}

function render(){
 const c=snap.counts||{};
 const pre=snap.pre_build;
 // ---- header (rows 0/1) ----
 let hdr='<div class="t"> Google Fonts library build'+(snap.paused?' [PAUSED]':'')+
   (pre?'<span class="right muted">first-time setup</span>':'<span class="right w">elapsed '+hms(snap.elapsed)+'</span>')+'</div>';
 if(pre){hdr+='<div class="sub"> configure your build below, then navigate to ▶ Start build</div>';}
 else{
  const bld=snap.disk_build_total||0,arc=snap.disk_archive_total||0;
  const disk=snap.disk_archive_nested?('disk used '+human(bld)+' (build + nested archive, all included)')
    :('disk used '+human(bld+arc)+' (build '+human(bld)+' + archive '+human(arc)+')');
  hdr+='<div class="sub"> '+disk+'  free '+human(snap.disk_free)+'  jobs '+(snap.jobs||0)+'  cohorts '+((snap.cohorts||[]).length)+
    '  fontc '+((snap.backends||{}).fontc||0)+'/fontmake '+((snap.backends||{}).fontmake||0)+'</div>';
 }
 document.getElementById('hdr').innerHTML=hdr;
 // ---- progress bar (rows 2/3) ----
 document.getElementById('bar').innerHTML=pre?'':barHTML();
 // ---- tabs (row 4) ----
 document.getElementById('tabs').innerHTML=TABS.map(t=>'<span class="tab'+(t==tab?' on':'')+'" onclick="setTab(\''+t+'\')">'+t+'</span>').join('')+
   '<span class="tabhint">click a tab to switch · polling every 1.5s</span>';
 // ---- controls ----
 document.getElementById('ctl').innerHTML=
   '<button onclick="ctl({paused:true})"'+(snap.paused?' disabled':'')+'>pause</button> '+
   '<button onclick="ctl({paused:false})"'+(snap.paused?'':' disabled')+'>resume</button>'+
   '<span class="muted"> &nbsp; hover a family row for a ↻ retry button · live edits go to control.json</span>';
 // ---- pinned now-building (every tab) ----
 const bl=snap.building||[];let pin='';
 if(bl.length&&!pre){const cap=Math.min(bl.length,5);
  pin='<div class="pin"><div class="sec">▶ Now building ('+bl.length+')</div>'+bl.slice(0,cap).map(b=>R(buildingRow(b).segs)).join('')+
   (bl.length>cap?'<div class="ln muted">  … (+'+(bl.length-cap)+' more)</div>':'')+'</div>';}
 document.getElementById('pin').innerHTML=pin;
 // ---- body per tab: charts (web-only) first, then the same content as the TUI ----
 let body=charts(tab);
 if(tab=='config')body+=cfgView();
 else if(tab=='archive')body+=archiveView();
 else{body+=(tab=='stats'?statsPrefix():'')+sections(tab).map(renderSec).join('');}
 document.getElementById('body').innerHTML=body;
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
 const pct=Math.floor(100*done/inscope);
 const gw=100*(c.built||0)/inscope,rw=100*(c.failed||0)/inscope,dw=Math.max(0,100-gw-rw);
 const skip=(c.skipped||0)?(' · '+(c.skipped||0)+' skipped'):'';
 const hint=(c.skipped||0)?'<span class="skip">'+(c.skipped||0)+' skipped (not selected — raise % to 100 to build them)</span>':'';
 return '<div class="phase"> Phase: '+E(phaseLabel(ph))+'   built '+(c.built||0)+'  failed '+(c.failed||0)+'  building '+(c.building||0)+'  queued '+(c.queued||0)+err+hint+'</div>'+
  '<div class="barwrap"><div class="seg bg" style="width:'+gw+'%"></div><div class="seg rg" style="width:'+rw+'%"></div><div class="seg dg" style="width:'+dw+'%"></div>'+
  '<div class="barlbl">'+done+'/'+inscope+' attempted ('+pct+'%)'+skip+'</div></div>';
}
function phaseLabel(ph){return {init:'starting…',clone_gf:'cloning google/fonts',build_fontc:'building fontc from source',discover:'discovering worklist',archive:'populating archive (mirroring repos)',cohorts:'scanning dependency cohorts',build:'building',done:'done'}[ph]||ph||''}

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
  '<text x="44" y="42" text-anchor="middle" fill="#fff" font-size="15" font-weight="600">'+Math.floor(100*frac)+'%</text>'+
  '<text x="44" y="58" text-anchor="middle" fill="#7c8aa0" font-size="9">'+E(sub||'')+'</text></svg>';
}
function legend(slices){return '<div class="legend">'+slices.filter(s=>(s.value||0)>0).map(s=>'<span><i style="background:'+s.color+'"></i>'+E(s.label)+' '+(s.value||0)+'</span>').join('')+'</div>'}

function charts(t){
 const c=snap.counts||{};
 if(t=='overview'){
  const sl=[{label:'built',value:c.built||0,color:'#22c55e'},{label:'failed',value:c.failed||0,color:'#ef4444'},{label:'building',value:c.building||0,color:'#06b6d4'},{label:'queued',value:c.queued||0,color:'#eab308'},{label:'skipped',value:c.skipped||0,color:'#475569'}];
  const fc=(snap.fail_categories||[]).slice().sort((a,b)=>b.count-a.count).slice(0,6).map(x=>({label:x.cat,value:x.count,color:'#ef4444'}));
  return '<div class="chartrow">'+chartCard('outcome','<div class="dwrap">'+donut(sl,52)+legend(sl)+'</div>')+chartCard('top failure causes',barChart(fc))+'</div>';
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
  return co.length?'<div class="chartrow">'+chartCard('cohort sizes (green = venv cached on disk)',barChart(co))+'</div>':'';
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
poll();setInterval(poll,1500);
</script></body></html>"###;
