//! Hosted experience surface (M1): renders the explainable trace, world model, and audit log
//! (PRD-002 §27 EXP-005, SAD §16). The native on-GPU compositor is P5; this is the hosted equivalent.
use crate::domain::EventRecord;
use crate::intent_action::Trace;
use crate::storage::Store;

const GUI_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Aletheia Experience</title>
<style>
:root{font-family:Inter,ui-sans-serif,system-ui,sans-serif;color:#e8edf5;background:#0b1018}*{box-sizing:border-box}body{margin:0;display:grid;grid-template-columns:230px 1fr;min-height:100vh}aside{border-right:1px solid #263244;padding:24px 16px;background:#0e1520}main{padding:28px;max-width:1500px;width:100%}h1,h2{margin:0 0 8px}.muted{color:#8794a8}.nav{display:grid;gap:6px;margin-top:24px}.nav button,.action{border:1px solid #2a394e;background:#111b29;color:#dfe8f5;border-radius:8px;padding:10px;text-align:left;cursor:pointer}.nav button.active{background:#1a2a3f}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px;margin:22px 0}.card{border:1px solid #263244;background:#101824;border-radius:12px;padding:16px}.value{font-size:28px;margin-top:8px}.toolbar{display:flex;gap:8px;flex-wrap:wrap;margin:18px 0}input,textarea,select{background:#0b121d;color:#e8edf5;border:1px solid #2a394e;border-radius:8px;padding:10px;width:100%}textarea{min-height:110px}table{width:100%;border-collapse:collapse}.scroll{overflow:auto}.scroll td,.scroll th{padding:10px;border-bottom:1px solid #263244;text-align:left;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;background:#0b121d;padding:14px;border-radius:8px}.danger{border-color:#7d3942}.status{padding:8px 10px;border-radius:8px;background:#152236;margin:10px 0}.hidden{display:none}.trace{display:grid;gap:7px}.trace div{display:grid;grid-template-columns:150px 1fr;gap:10px;border-bottom:1px solid #202d3f;padding:8px}.trace b{color:#8fa2bc}
</style></head><body><aside><h2>Aletheia</h2><div class="muted">Experience surface</div><div class="nav" id="nav"></div></aside><main>
<section id="dashboard"><h1>System overview</h1><div class="muted">Capability-gated semantic desktop</div><div class="grid" id="stats"></div><div class="card"><h2>Intent</h2><textarea id="intent" placeholder="Describe what you want Aletheia to do"></textarea><div class="toolbar"><button class="action" onclick="submitIntent(false)">Plan</button><button class="action danger" onclick="submitIntent(true)">Execute / approve</button></div><div id="intentResult"></div></div></section>
<section id="world" class="hidden"><h1>World model</h1><div class="toolbar"><input id="search" placeholder="Search entities by meaning / keywords" onkeydown="if(event.key==='Enter')doSearch()"><button class="action" onclick="doSearch()">Search</button></div><div id="worldBody"></div></section>
<section id="capabilities" class="hidden"><h1>Capabilities</h1><div class="muted">Bearer tokens are never rendered.</div><div id="capsBody" class="card"></div></section>
<section id="approvals" class="hidden"><h1>Approvals</h1><div id="approvalsBody" class="card"></div></section>
<section id="audit" class="hidden"><h1>Audit</h1><div id="auditBody" class="card"></div></section>
<section id="trace" class="hidden"><h1>Action trace</h1><div id="traceBody" class="card trace"></div></section>
<section id="setup"><h1>Session</h1><div class="muted">Bootstrap root capability once, then keep it in this browser session.</div><div class="toolbar"><input id="subject" value="human:operator" placeholder="subject"><button class="action" onclick="bootstrap()">Bootstrap</button><button class="action danger" onclick="logout()">Forget session</button></div><div id="setupStatus" class="status"></div></section>
</main><script>
const tabs=['dashboard','world','capabilities','approvals','audit','trace','setup'];let token=sessionStorage.getItem('aletheia.token')||'';
const nav=document.getElementById('nav');tabs.forEach((x,i)=>{let b=document.createElement('button');b.textContent=x[0].toUpperCase()+x.slice(1);b.onclick=()=>show(x);if(i===0)b.classList.add('active');nav.appendChild(b)});
function show(id){tabs.forEach(x=>document.getElementById(x).classList.toggle('hidden',x!==id));[...nav.children].forEach((b,i)=>b.classList.toggle('active',tabs[i]===id));if(id==='dashboard')loadStats();if(id==='world')loadWorld();if(id==='capabilities')loadCaps();if(id==='approvals')loadApprovals();if(id==='audit')loadAudit()}
async function call(op){let r=await fetch('/api/request',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({...op,caps:op.caps||[token]})});let j=await r.json();if(!j.ok)throw Error(j.error||'request failed');return j.data}
async function bootstrap(){try{let d=await call({op:'BootstrapOwner',subject:document.getElementById('subject').value,caps:[]});token=d.token;sessionStorage.setItem('aletheia.token',token);document.getElementById('setupStatus').textContent='Session ready for '+d.subject;loadStats()}catch(e){document.getElementById('setupStatus').textContent=e.message}}
async function loadStats(){if(!token){document.getElementById('stats').innerHTML='<div class="card">Bootstrap session first.</div>';return}try{let [a,p,c,w]=await Promise.all([call({op:'QueryAudit',limit:100}),call({op:'ListApprovals'}),call({op:'QueryCapabilities'}),call({op:'QueryWorld'})]);document.getElementById('stats').innerHTML=`<div class=card>Entities<div class=value>${w.entities.length}</div></div><div class=card>Relationships<div class=value>${w.relationships.length}</div></div><div class=card>Capabilities<div class=value>${c.length}</div></div><div class=card>Pending approvals<div class=value>${p.length}</div></div><div class=card>Audit events<div class=value>${a.length}</div></div>`}catch(e){document.getElementById('stats').innerHTML='<div class="card">'+e.message+'</div>'}}
async function loadWorld(){try{let w=await call({op:'QueryWorld'});renderWorld(w.entities,w.relationships)}catch(e){document.getElementById('worldBody').textContent=e.message}}
function renderWorld(es,rs){document.getElementById('worldBody').innerHTML='<div class=card><h2>Entities</h2><div class=scroll><table><tr><th>ID</th><th>Type</th><th>Version</th><th>Metadata</th></tr>'+es.map(e=>`<tr><td>${esc(e.id)}</td><td>${esc(e.etype)}</td><td>${e.version}</td><td><pre>${esc(JSON.stringify(e.metadata||{},null,2))}</pre></td></tr>`).join('')+'</table></div></div><div class=card><h2>Relationships</h2><pre>'+esc(JSON.stringify(rs,null,2))+'</pre></div>'}
async function doSearch(){try{let h=await call({op:'Search',query:document.getElementById('search').value,limit:20});document.getElementById('worldBody').innerHTML='<div class=card><h2>Search results</h2><pre>'+esc(JSON.stringify(h,null,2))+'</pre></div>'}catch(e){document.getElementById('worldBody').textContent=e.message}}
async function loadCaps(){try{let c=await call({op:'QueryCapabilities'});document.getElementById('capsBody').innerHTML='<pre>'+esc(JSON.stringify(c,null,2))+'</pre>'}catch(e){document.getElementById('capsBody').textContent=e.message}}
async function loadApprovals(){try{let p=await call({op:'ListApprovals'});document.getElementById('approvalsBody').innerHTML=p.length?p.map(x=>`<div class=card><b>${esc(x.id)}</b><pre>${esc(JSON.stringify(x.intent,null,2))}</pre><button class=action onclick="resolve('${esc(x.id)}',true)">Grant</button> <button class="action danger" onclick="resolve('${esc(x.id)}',false)">Deny</button></div>`).join(''):'No pending approvals.'}catch(e){document.getElementById('approvalsBody').textContent=e.message}}
async function resolve(id,granted){try{let t=await call({op:'ResolveApproval',approval_id:id,granted});show('trace');renderTrace(t)}catch(e){alert(e.message)}}
async function loadAudit(){try{let a=await call({op:'QueryAudit',limit:100});document.getElementById('auditBody').innerHTML='<pre>'+esc(JSON.stringify(a,null,2))+'</pre>'}catch(e){document.getElementById('auditBody').textContent=e.message}}
async function submitIntent(approve){try{let subject=document.getElementById('subject').value;let text=document.getElementById('intent').value;let d=await call({op:'SubmitIntent',intent:{subject,verb:{Read:{id:text}}},approve});document.getElementById('intentResult').innerHTML='<pre>'+esc(JSON.stringify(d,null,2))+'</pre>';renderTrace(d);show('trace')}catch(e){document.getElementById('intentResult').textContent=e.message}}
function renderTrace(t){document.getElementById('traceBody').innerHTML='<div><b>subject</b><span>'+esc(t.subject)+'</span></div><div><b>intent</b><span>'+esc(t.intent)+'</span></div><div><b>context</b><span>'+esc((t.context_provenance||[]).join(', ')||'(none)')+'</span></div><div><b>interpreter</b><span>'+esc(t.interpreter)+'</span></div><div><b>plan</b><span>'+esc(t.proposed_plan_raw||'')+'</span></div><div><b>validation</b><span>'+esc(t.validation)+'</span></div><div><b>capability</b><span>'+esc(t.capability_decision)+'</span></div><div><b>approval</b><span>'+esc(t.approval)+'</span></div><div><b>execution</b><span>'+esc(t.execution)+'</span></div><div><b>verification</b><span>'+esc(t.verification)+'</span></div><div><b>result</b><span>'+esc(JSON.stringify(t.result))+'</span></div><div><b>outcome</b><span>'+String(t.ok)+'</span></div>'}
function esc(s){return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
loadStats();
</script></body></html>"##;

/// Run the hosted Experience GUI. Bound to loopback so browser access stays local; all state-changing
/// operations still traverse the same capability-gated CoreService request boundary.
pub fn serve_gui(mut service: crate::service::CoreService, bind: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind(bind)?;
    eprintln!("experience GUI: http://{bind}");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buf = [0u8; 65536];
        let n = match stream.read(&mut buf) { Ok(n) => n, Err(_) => continue };
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        let (method, path) = first.split_once(' ').map(|(a,b)| (a,b.split(' ').next().unwrap_or(""))).unwrap_or(("", ""));
        let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
        let (status, content_type, payload) = match (method, path) {
            ("GET", "/") => ("200 OK", "text/html; charset=utf-8", GUI_HTML.to_string()),
            ("POST", "/api/request") => match serde_json::from_str::<crate::service::Request>(body) {
                Ok(r) => {
                    let response = service.handle(r);
                    ("200 OK", "application/json", serde_json::to_string(&response).unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialization failure\"}".into()))
                }
                Err(e) => ("400 Bad Request", "application/json", serde_json::json!({"ok":false,"error":format!("bad request: {e}")}).to_string()),
            },
            _ => ("404 Not Found", "text/plain; charset=utf-8", "not found".into()),
        };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n{payload}", payload.len());
        let _ = stream.write_all(response.as_bytes());
    }
    Ok(())
}

pub fn render_trace(t: &Trace) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "+- Action trace [{}]  subject={}\n",
        short(&t.correlation_id),
        t.subject
    ));
    s.push_str(&format!("| intent          {}\n", t.intent));
    s.push_str(&format!(
        "| context         {}\n",
        if t.context_provenance.is_empty() {
            "(none)".into()
        } else {
            t.context_provenance.join(", ")
        }
    ));
    s.push_str(&format!("| interpreter     {}\n", t.interpreter));
    s.push_str(&format!(
        "| proposed plan   {}\n",
        truncate(&t.proposed_plan_raw, 100)
    ));
    s.push_str(&format!("| validation      {}\n", t.validation));
    s.push_str(&format!("| capability      {}\n", t.capability_decision));
    s.push_str(&format!("| approval        {}\n", t.approval));
    s.push_str(&format!("| execution       {}\n", t.execution));
    s.push_str(&format!("| verification    {}\n", t.verification));
    s.push_str(&format!(
        "| result          {}\n",
        truncate(&t.result.to_string(), 160)
    ));
    if let Some(e) = &t.error {
        s.push_str(&format!("| error           {}\n", e));
    }
    s.push_str(&format!(
        "+- outcome        {}\n",
        if t.ok {
            "OK"
        } else {
            "STOPPED (no unsafe effect)"
        }
    ));
    s
}

pub fn render_world(store: &Store) -> String {
    let mut s = String::from("World model (relationships):\n");
    for r in store.relationships() {
        s.push_str(&format!(
            "  {}  --{}-->  {}\n",
            short(&r.from),
            r.rtype,
            short(&r.to)
        ));
    }
    s
}

pub fn render_audit(store: &Store) -> String {
    let mut s = String::from("Audit log (immutable events):\n");
    for ev in store.events() {
        s.push_str(&format!(
            "  [{}] {} by {}\n",
            short(&ev.correlation_id),
            ev.etype,
            ev.actor
        ));
    }
    s
}

pub fn render_event(ev: &EventRecord) -> String {
    format!("{} {} {}", ev.etype, ev.actor, ev.payload)
}

fn short(id: &str) -> String {
    if id.len() > 8 {
        id[id.len() - 8..].to_string()
    } else {
        id.to_string()
    }
}
fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}...", &s[..n])
    } else {
        s.to_string()
    }
}
