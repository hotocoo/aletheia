//! Hosted experience surface (M1): renders the explainable trace, world model, and audit log
//! (PRD-002 §27 EXP-005, SAD §16). The native on-GPU compositor is P5; this is the hosted equivalent.
use crate::domain::EventRecord;
use crate::intent_action::Trace;
use crate::storage::Store;

const GUI_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Aletheia Experience</title>
<style nonce="__CSP_NONCE__">
:root{font-family:Inter,ui-sans-serif,system-ui,sans-serif;color:#e8edf5;background:#0b1018}*{box-sizing:border-box}html{scroll-behavior:smooth}body{margin:0;display:grid;grid-template-columns:230px 1fr;min-height:100vh}aside{border-right:1px solid #263244;padding:24px 16px;background:#0e1520;position:sticky;top:0;height:100vh}.brand{display:flex;align-items:center;justify-content:space-between;gap:8px}.shortcut{font-size:11px;color:#8794a8;border:1px solid #263244;border-radius:6px;padding:3px 5px}.session-dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:#65758a;margin-right:6px}.session-dot.ready{background:#63b37a}.nav{display:grid;gap:6px;margin-top:24px}.nav button,.action{border:1px solid #2a394e;background:#111b29;color:#dfe8f5;border-radius:8px;padding:10px;text-align:left;cursor:pointer}.nav button:hover,.action:hover{border-color:#536b89}.nav button:focus-visible,.action:focus-visible,input:focus-visible,textarea:focus-visible{outline:2px solid #8fa2bc;outline-offset:2px}.nav button.active{background:#1a2a3f}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px;margin:22px 0}.card{border:1px solid #263244;background:#101824;border-radius:12px;padding:16px}.value{font-size:28px;margin-top:8px}.toolbar{display:flex;gap:8px;flex-wrap:wrap;margin:18px 0}.toolbar input{flex:1;min-width:220px}input,textarea,select{background:#0b121d;color:#e8edf5;border:1px solid #2a394e;border-radius:8px;padding:10px;width:100%}textarea{min-height:110px;resize:vertical}table{width:100%;border-collapse:collapse}.scroll{overflow:auto}.scroll td,.scroll th{padding:10px;border-bottom:1px solid #263244;text-align:left;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;background:#0b121d;padding:14px;border-radius:8px}.danger{border-color:#7d3942}.status{padding:8px 10px;border-radius:8px;background:#152236;margin:10px 0}.status.error{border:1px solid #7d3942}.hidden{display:none}.trace{display:grid;gap:7px}.trace div{display:grid;grid-template-columns:150px 1fr;gap:10px;border-bottom:1px solid #202d3f;padding:8px}.trace b{color:#8fa2bc}.loading{opacity:.65;pointer-events:none}.palette{position:fixed;inset:0;background:rgba(0,0,0,.55);display:grid;place-items:start center;padding-top:14vh}.palette-card{width:min(680px,calc(100vw - 32px));border:1px solid #3b4d65;background:#101824;border-radius:12px;padding:12px;box-shadow:0 18px 50px rgba(0,0,0,.45)}.palette-card input{margin-bottom:8px}.palette-card .selected{border-color:#8fa2bc;background:#1a2a3f}.toast{position:fixed;right:20px;bottom:20px;max-width:420px;border:1px solid #3b4d65;background:#101824;padding:12px 14px;border-radius:8px;box-shadow:0 10px 30px rgba(0,0,0,.35);z-index:20}@media(max-width:760px){body{display:block}aside{position:sticky;top:0;height:auto;z-index:5;border-right:0;border-bottom:1px solid #263244;padding:12px 14px}.brand h2{margin:0}.nav{display:flex;overflow-x:auto;gap:6px;margin-top:12px;padding-bottom:2px}.nav button{flex:0 0 auto;white-space:nowrap}main{padding:0 12px}.main-head{display:block}.toolbar input{min-width:0}.grid{grid-template-columns:1fr 1fr}.trace div{grid-template-columns:1fr;gap:4px}}@media(max-width:480px){.grid{grid-template-columns:1fr}.shortcut{display:none}.palette{padding-top:8vh}.palette-card{width:calc(100vw - 20px)}}@media(prefers-reduced-motion:reduce){html{scroll-behavior:auto}}
</style></head><body><a class="hidden" href="#main-content">Skip to content</a><aside><div class="brand"><h2>Aletheia</h2><span class="shortcut">Ctrl/⌘ K</span></div><div class="muted"><span id="sessionDot" class="session-dot" aria-hidden="true"></span><span id="sessionLabel">No session</span></div><nav class="nav" id="nav" aria-label="Experience surfaces"></nav></aside><main id="main-content">
<section id="dashboard"><div class="main-head"><div><h1>System overview</h1><div class="muted">Capability-gated semantic desktop</div></div><div class="toolbar"><button class="action" onclick="refreshCurrent()">Refresh</button><button class="action" onclick="openPalette()">Command palette</button></div></div><div class="grid" id="stats"></div><div class="card"><h2>Intent</h2><textarea id="intent" placeholder="Describe what you want Aletheia to do"></textarea><div class="toolbar"><button class="action" onclick="submitIntent(false)">Plan</button><button class="action danger" onclick="submitIntent(true)">Execute / approve</button></div><div id="intentResult"></div></div></section>
<section id="world" class="hidden"><h1>World model</h1><div class="toolbar"><input id="search" placeholder="Search entities by meaning / keywords" onkeydown="if(event.key==='Enter')doSearch()"><button class="action" onclick="doSearch()">Search</button></div><div id="worldBody"></div></section>
<section id="capabilities" class="hidden"><h1>Capabilities</h1><div class="muted">Bearer tokens are never rendered.</div><div id="capsBody" class="card"></div></section>
<section id="approvals" class="hidden"><h1>Approvals</h1><div id="approvalsBody" class="card"></div></section>
<section id="audit" class="hidden"><h1>Audit</h1><div id="auditBody" class="card"></div></section>
<section id="trace" class="hidden"><h1>Action trace</h1><div id="traceBody" class="card trace"></div></section>
<section id="performance" class="hidden"><h1>Performance</h1><div class="muted">Live request-dispatch telemetry for the hosted Core. These figures measure Aletheia's service path; they are not CPU-frequency or thermal measurements.</div><div class="grid" id="perfStats"></div><div class="card"><h2>Interpretation</h2><p>Use this surface to catch regressions in the Core boundary. Hardware frequency, package power, thermals, and device-specific utilization require the native hardware backend and are intentionally not fabricated here.</p></div></section>
<section id="setup"><h1>Session</h1><div class="muted">Bootstrap root capability once, then keep it in this browser session.</div><div class="toolbar"><input id="subject" value="human:operator" placeholder="subject"><button class="action" onclick="bootstrap()">Bootstrap</button><button class="action danger" onclick="logout()">Forget session</button></div><div id="setupStatus" class="status" role="status" aria-live="polite"></div></section>
</main><div id="toast" class="toast hidden" role="status" aria-live="polite"></div><div id="palette" class="palette hidden" role="dialog" aria-modal="true" aria-label="Command palette"><div class="palette-card"><input id="paletteInput" autocomplete="off" placeholder="Jump to a surface or action…"><div id="paletteList"></div></div></div><script nonce="__CSP_NONCE__">
const tabs=['dashboard','world','capabilities','approvals','audit','trace','performance','setup'];let token=sessionStorage.getItem('aletheia.token')||'';let paletteIndex=0;let toastTimer;let activeTab=token?(sessionStorage.getItem('aletheia.surface')||'dashboard'):'setup';
const nav=document.getElementById('nav');let paletteReturnFocus=null;tabs.forEach((x,i)=>{let b=document.createElement('button');b.type='button';b.textContent=x[0].toUpperCase()+x.slice(1);b.onclick=()=>show(x);b.setAttribute('aria-controls',x);if(i===0)b.classList.add('active');nav.appendChild(b)});
function updateSessionIndicator(){let ready=Boolean(token);document.getElementById('sessionDot').classList.toggle('ready',ready);document.getElementById('sessionLabel').textContent=ready?'Session ready':'No session'}
function show(id){activeTab=id;sessionStorage.setItem('aletheia.surface',id);tabs.forEach(x=>document.getElementById(x).classList.toggle('hidden',x!==id));[...nav.children].forEach((b,i)=>{let active=tabs[i]===id;b.classList.toggle('active',active);if(active)b.setAttribute('aria-current','page');else b.removeAttribute('aria-current')});if(id==='dashboard')loadStats();if(id==='world')loadWorld();if(id==='capabilities')loadCaps();if(id==='approvals')loadApprovals();if(id==='audit')loadAudit();if(id==='performance')loadPerformance()}
function notify(message){let el=document.getElementById('toast');el.textContent=message;el.classList.remove('hidden');clearTimeout(toastTimer);toastTimer=setTimeout(()=>el.classList.add('hidden'),3000)}
function refreshCurrent(){let active=activeTab;if(active==='dashboard')loadStats();else if(active==='world')loadWorld();else if(active==='capabilities')loadCaps();else if(active==='approvals')loadApprovals();else if(active==='audit')loadAudit();else if(active==='performance')loadPerformance();notify('Refreshed '+active)}
const paletteActions=[...tabs.map(x=>({label:'Open '+x[0].toUpperCase()+x.slice(1),run:()=>show(x)})),{label:'Refresh current surface',run:refreshCurrent},{label:'Focus intent editor',run:()=>{show('dashboard');document.getElementById('intent').focus()}}];
function openPalette(){paletteReturnFocus=document.activeElement;document.getElementById('palette').classList.remove('hidden');document.getElementById('paletteInput').value='';paletteIndex=0;renderPalette();document.getElementById('paletteInput').focus()}
function closePalette(){document.getElementById('palette').classList.add('hidden');if(paletteReturnFocus&&typeof paletteReturnFocus.focus==='function')paletteReturnFocus.focus();paletteReturnFocus=null}
function renderPalette(){let q=document.getElementById('paletteInput').value.toLowerCase();let items=paletteActions.filter(a=>a.label.toLowerCase().includes(q));paletteIndex=Math.max(0,Math.min(paletteIndex,Math.max(items.length-1,0)));document.getElementById('paletteList').innerHTML=items.map((a,i)=>`<button class="action ${i===paletteIndex?'selected':''}" onclick="runPalette(${paletteActions.indexOf(a)})">${esc(a.label)}</button>`).join('')||'<div class="muted">No matching commands.</div>'}
function runPalette(i){closePalette();paletteActions[i].run()}
document.getElementById('paletteInput').addEventListener('input',renderPalette);
document.getElementById('palette').addEventListener('click',e=>{if(e.target===e.currentTarget)closePalette()});
document.addEventListener('keydown',e=>{if((e.metaKey||e.ctrlKey)&&e.key.toLowerCase()==='k'){e.preventDefault();openPalette();return}if(document.getElementById('palette').classList.contains('hidden'))return;if(e.key==='Escape'){closePalette();return}let q=document.getElementById('paletteInput').value.toLowerCase();let items=paletteActions.filter(a=>a.label.toLowerCase().includes(q));if(e.key==='ArrowDown'){e.preventDefault();paletteIndex=(paletteIndex+1)%Math.max(items.length,1);renderPalette()}else if(e.key==='ArrowUp'){e.preventDefault();paletteIndex=(paletteIndex-1+Math.max(items.length,1))%Math.max(items.length,1);renderPalette()}else if(e.key==='Enter'&&items[paletteIndex]){e.preventDefault();items[paletteIndex].run();closePalette()}});
async function call(op){let r;try{r=await fetch('/api/request',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({...op,caps:op.caps||[token]})})}catch(e){throw Error('Core service unavailable; check the local session')};let j;try{j=await r.json()}catch(_){throw Error('request returned invalid JSON ('+r.status+')')}if(!r.ok||!j.ok){if(r.status===401||r.status===403){token='';sessionStorage.removeItem('aletheia.token');updateSessionIndicator()}throw Error(j.error||('request failed ('+r.status+')'))}return j.data}
function busy(el,on){if(!el)return;el.classList.toggle('loading',on);el.setAttribute('aria-busy',on?'true':'false')}
function setStatus(el,message,error=false){el.textContent=message;el.classList.toggle('error',error)}
function logout(){token='';sessionStorage.removeItem('aletheia.token');sessionStorage.removeItem('aletheia.surface');updateSessionIndicator();setStatus(document.getElementById('setupStatus'),'Session forgotten.');document.getElementById('stats').innerHTML='<div class="card">Bootstrap session first.</div>';show('setup')}
async function bootstrap(){let status=document.getElementById('setupStatus');try{setStatus(status,'Bootstrapping…');let d=await call({op:'BootstrapOwner',subject:document.getElementById('subject').value.trim()||'human:operator',caps:[]});token=d.token;sessionStorage.setItem('aletheia.token',token);updateSessionIndicator();setStatus(status,'Session ready for '+d.subject);show('dashboard')}catch(e){setStatus(status,e.message,true)}}
async function loadStats(){if(!token){document.getElementById('stats').innerHTML='<div class="card">Bootstrap session first.</div>';return}let el=document.getElementById('stats');busy(el,true);try{let [a,p,c,w]=await Promise.all([call({op:'QueryAudit',limit:100}),call({op:'ListApprovals'}),call({op:'QueryCapabilities'}),call({op:'QueryWorld'})]);el.innerHTML=`<div class=card>Entities<div class=value>${w.entities.length}</div></div><div class=card>Relationships<div class=value>${w.relationships.length}</div></div><div class=card>Capabilities<div class=value>${c.length}</div></div><div class=card>Pending approvals<div class=value>${p.length}</div></div><div class=card>Audit events<div class=value>${a.length}</div></div>`}catch(e){el.innerHTML='<div class="card">'+esc(e.message)+'</div>'}finally{busy(el,false)}}
async function loadWorld(){let el=document.getElementById('worldBody');el.innerHTML='<div class=card role=status aria-live=polite>Loading world model…</div>';try{let w=await call({op:'QueryWorld'});renderWorld(w.entities,w.relationships)}catch(e){el.innerHTML='<div class="card status error" role=alert>'+esc(e.message)+'</div>'}}
function renderWorld(es,rs){let entityRows=es.map(e=>`<tr><td>${esc(e.id)}</td><td>${esc(e.etype)}</td><td>${e.version}</td><td><pre>${esc(JSON.stringify(e.metadata||{},null,2))}</pre></td></tr>`).join('');let relationshipJson=JSON.stringify(rs,null,2);document.getElementById('worldBody').innerHTML='<div class=card><h2>Entities</h2>'+ (entityRows?'<div class=scroll><table><tr><th>ID</th><th>Type</th><th>Version</th><th>Metadata</th></tr>'+entityRows+'</table></div>':'<div class="status">No entities are visible to this session.</div>')+'</div><div class=card><h2>Relationships</h2>'+ (rs.length?'<pre>'+esc(relationshipJson)+'</pre>':'<div class="status">No relationships are visible to this session.</div>')+'</div>'}
async function doSearch(){try{let h=await call({op:'Search',query:document.getElementById('search').value,limit:20});document.getElementById('worldBody').innerHTML='<div class=card><h2>Search results</h2><pre>'+esc(JSON.stringify(h,null,2))+'</pre></div>'}catch(e){document.getElementById('worldBody').textContent=e.message}}
async function loadCaps(){let el=document.getElementById('capsBody');el.innerHTML='<div role=status>Loading capabilities…</div>';try{let c=await call({op:'QueryCapabilities'});el.innerHTML='<pre>'+esc(JSON.stringify(c,null,2))+'</pre>'}catch(e){el.innerHTML='<div class="status error" role=alert>'+esc(e.message)+'</div>'}}
async function loadApprovals(){let el=document.getElementById('approvalsBody');el.innerHTML='<div role=status>Loading approvals…</div>';try{let p=await call({op:'ListApprovals'});el.innerHTML=p.length?p.map(x=>`<div class=card><b>${esc(x.id)}</b><pre>${esc(JSON.stringify(x.intent,null,2))}</pre><button class=action data-approval="${esc(x.id)}" data-granted="true">Grant</button> <button class="action danger" data-approval="${esc(x.id)}" data-granted="false">Deny</button></div>`).join(''):'<div class=status>No pending approvals.</div>'}catch(e){el.innerHTML='<div class="status error" role=alert>'+esc(e.message)+'</div>'}}
async function resolve(id,granted){try{let t=await call({op:'ResolveApproval',approval_id:id,granted});show('trace');renderTrace(t)}catch(e){alert(e.message)}}
async function loadAudit(){let el=document.getElementById('auditBody');el.innerHTML='<div role=status>Loading audit events…</div>';try{let a=await call({op:'QueryAudit',limit:100});el.innerHTML=a.length?'<pre>'+esc(JSON.stringify(a,null,2))+'</pre>':'<div class=status>No audit events are visible to this session.</div>'}catch(e){el.innerHTML='<div class="status error" role=alert>'+esc(e.message)+'</div>'}}
async function loadPerformance(){let el=document.getElementById('perfStats');if(!token){el.innerHTML='<div class="card">Bootstrap session first.</div>';return}busy(el,true);try{let p=await call({op:'QueryPerformance'});let ns=v=>Number(v||0).toLocaleString();el.innerHTML=`<div class=card>Requests<div class=value>${ns(p.requests)}</div></div><div class=card>Average latency<div class=value>${ns(p.average_ns)} ns</div></div><div class=card>P95 latency<div class=value>${ns(p.p95_ns)} ns</div></div><div class=card>P99 latency<div class=value>${ns(p.p99_ns)} ns</div></div><div class=card>Max latency<div class=value>${ns(p.max_ns)} ns</div></div><div class=card>Rolling samples<div class=value>${ns(p.sample_window)}</div></div>`}catch(e){el.innerHTML='<div class="card status error" role=alert>'+esc(e.message)+'</div>'}finally{busy(el,false)}}
async function submitIntent(approve){try{let subject=document.getElementById('subject').value;let text=document.getElementById('intent').value;let d=await call({op:'SubmitIntent',intent:{subject,verb:{Read:{id:text}}},approve});document.getElementById('intentResult').innerHTML='<pre>'+esc(JSON.stringify(d,null,2))+'</pre>';renderTrace(d);show('trace')}catch(e){document.getElementById('intentResult').textContent=e.message}}
function renderTrace(t){document.getElementById('traceBody').innerHTML='<div><b>subject</b><span>'+esc(t.subject)+'</span></div><div><b>intent</b><span>'+esc(t.intent)+'</span></div><div><b>context</b><span>'+esc((t.context_provenance||[]).join(', ')||'(none)')+'</span></div><div><b>interpreter</b><span>'+esc(t.interpreter)+'</span></div><div><b>plan</b><span>'+esc(t.proposed_plan_raw||'')+'</span></div><div><b>validation</b><span>'+esc(t.validation)+'</span></div><div><b>capability</b><span>'+esc(t.capability_decision)+'</span></div><div><b>approval</b><span>'+esc(t.approval)+'</span></div><div><b>execution</b><span>'+esc(t.execution)+'</span></div><div><b>verification</b><span>'+esc(t.verification)+'</span></div><div><b>result</b><span>'+esc(JSON.stringify(t.result))+'</span></div><div><b>outcome</b><span>'+String(t.ok)+'</span></div>'}
function esc(s){return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
updateSessionIndicator();show(activeTab);if(!token)setStatus(document.getElementById('setupStatus'),'No active session. Bootstrap a root capability to begin.');
document.getElementById('approvalsBody').addEventListener('click',e=>{let button=e.target.closest('button[data-approval]');if(!button)return;resolve(button.dataset.approval,button.dataset.granted==='true')});
</script></body></html>"##;

/// Maximum HTTP request size accepted by the hosted GUI. The parser refuses a larger
/// `Content-Length` before reading the body, so an attacker cannot turn the fixed receive buffer
/// into an allocation or truncation oracle.
const MAX_GUI_REQUEST: usize = 64 * 1024;

/// Run the hosted Experience GUI. Bound to loopback so browser access stays local; all state-changing
/// operations still traverse the same capability-gated CoreService request boundary. The HTTP seam
/// is deliberately small and fail-closed: bounded headers/body, explicit same-origin checks for
/// browser requests, and no permissive CORS path.
pub fn serve_gui(mut service: crate::service::CoreService, bind: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind(bind)?;
    let expected_origin = format!("http://{bind}");
    eprintln!("experience GUI: http://{bind}");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
        let mut buf = [0u8; 65536];
        let mut n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        // Header parsing is incremental: TCP is a byte stream, so one read is not a request.
        // Keep the receive path allocation-free and refuse an unterminated header once the fixed
        // request ceiling is exhausted.
        while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") && n < buf.len() {
            match stream.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(m) => n += m,
                Err(_) => break,
            }
        }
        if !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            let response =
                "HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            continue;
        }
        let header_end = buf[..n]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("request header terminator was checked above")
            + 4;
        let content_length = {
            let req = match std::str::from_utf8(&buf[..header_end]) {
                Ok(req) => req,
                Err(_) => {
                    let response =
                        "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                    continue;
                }
            };
            req[..header_end - 4]
                .lines()
                .skip(1)
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim())
                })
                .map(|value| value.parse::<usize>().ok())
                .unwrap_or(Some(0))
        };
        if let Some(len) = content_length {
            if len > MAX_GUI_REQUEST || header_end.saturating_add(len) > buf.len() {
                let response = "HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                continue;
            }
            let target = header_end + len;
            while n < target {
                match stream.read(&mut buf[n..target]) {
                    Ok(0) => break,
                    Ok(m) => n += m,
                    Err(_) => break,
                }
            }
        }
        let req = match std::str::from_utf8(&buf[..n]) {
            Ok(req) => req,
            Err(_) => {
                let response =
                    "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                continue;
            }
        };
        let first = req.lines().next().unwrap_or("");
        let (method, path) = first
            .split_once(' ')
            .map(|(a, b)| (a, b.split(' ').next().unwrap_or("")))
            .unwrap_or(("", ""));
        let headers = &req[..header_end - 4];
        let body = &req[header_end..n];
        let origin_ok = headers
            .lines()
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("origin")
                    .then(|| value.trim())
            })
            .map(|origin| origin == expected_origin)
            .unwrap_or(true);
        let framing_ok = content_length
            .map(|len| len == body.len() && len <= MAX_GUI_REQUEST)
            .unwrap_or(false);
        let (status, content_type, payload) = match (method, path) {
            ("GET", "/") if origin_ok => {
                // A fresh per-response nonce lets the GUI keep its self-contained HTML while
                // removing `unsafe-inline` from the CSP.  The nonce is only a presentation
                // capability for these two trusted inline blocks; it is never returned by the
                // Core API or stored in the browser session.
                let nonce = {
                    let bytes: [u8; 16] = rand::random();
                    let mut out = String::with_capacity(32);
                    for byte in bytes {
                        use core::fmt::Write as _;
                        let _ = write!(&mut out, "{byte:02x}");
                    }
                    out
                };
                let html = GUI_HTML.replace("__CSP_NONCE__", &nonce);
                ("200 OK", "text/html; charset=utf-8", html)
            }
            ("POST", "/api/request") if origin_ok && framing_ok => {
                match serde_json::from_str::<crate::service::Request>(body) {
                    Ok(r) => {
                        let response = service.handle(r);
                        let status = if response.ok {
                            "200 OK"
                        } else {
                            "403 Forbidden"
                        };
                        (
                            status,
                            "application/json",
                            serde_json::to_string(&response).unwrap_or_else(|_| {
                                "{\"ok\":false,\"error\":\"serialization failure\"}".into()
                            }),
                        )
                    }
                    Err(e) => (
                        "400 Bad Request",
                        "application/json",
                        serde_json::json!({"ok":false,"error":format!("bad request: {e}")})
                            .to_string(),
                    ),
                }
            }
            (_, _) if !origin_ok => (
                "403 Forbidden",
                "text/plain; charset=utf-8",
                "cross-origin request refused".into(),
            ),
            (_, _) if !framing_ok => (
                "413 Payload Too Large",
                "text/plain; charset=utf-8",
                "request body exceeds the GUI limit or has invalid framing".into(),
            ),
            _ => (
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found".into(),
            ),
        };
        let csp = if method == "GET" && path == "/" && status == "200 OK" {
            // The nonce has already been embedded in the trusted HTML. Extracting it here keeps
            // the wire contract allocation-free for all API/error responses and avoids a second
            // random value that would invalidate the document's policy.
            let nonce = payload
                .split("nonce=\"")
                .nth(1)
                .and_then(|v| v.split('\"').next())
                .unwrap_or("");
            format!("default-src 'self'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'")
        } else {
            "default-src 'self'; script-src 'none'; style-src 'none'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'".to_string()
        };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nCross-Origin-Opener-Policy: same-origin\r\nContent-Security-Policy: {csp}\r\nCross-Origin-Resource-Policy: same-origin\r\nPermissions-Policy: camera=(), microphone=(), geolocation=(), usb=()\r\n\r\n{payload}", payload.len());
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
