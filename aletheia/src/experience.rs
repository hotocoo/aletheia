//! Hosted experience surface (M1): renders the explainable trace, world model, and audit log
//! (PRD-002 §27 EXP-005, SAD §16). The native on-GPU compositor is P5; this is the hosted equivalent.
use crate::domain::EventRecord;
use crate::intent_action::Trace;
use crate::storage::Store;

pub fn render_trace(t: &Trace) -> String {
    let mut s = String::new();
    s.push_str(&format!("+- Action trace [{}]  subject={}\n", short(&t.correlation_id), t.subject));
    s.push_str(&format!("| intent          {}\n", t.intent));
    s.push_str(&format!(
        "| context         {}\n",
        if t.context_provenance.is_empty() { "(none)".into() } else { t.context_provenance.join(", ") }
    ));
    s.push_str(&format!("| interpreter     {}\n", t.interpreter));
    s.push_str(&format!("| proposed plan   {}\n", truncate(&t.proposed_plan_raw, 100)));
    s.push_str(&format!("| validation      {}\n", t.validation));
    s.push_str(&format!("| capability      {}\n", t.capability_decision));
    s.push_str(&format!("| approval        {}\n", t.approval));
    s.push_str(&format!("| execution       {}\n", t.execution));
    s.push_str(&format!("| verification    {}\n", t.verification));
    s.push_str(&format!("| result          {}\n", truncate(&t.result.to_string(), 160)));
    if let Some(e) = &t.error {
        s.push_str(&format!("| error           {}\n", e));
    }
    s.push_str(&format!("+- outcome        {}\n", if t.ok { "OK" } else { "STOPPED (no unsafe effect)" }));
    s
}

pub fn render_world(store: &Store) -> String {
    let mut s = String::from("World model (relationships):\n");
    for r in store.relationships() {
        s.push_str(&format!("  {}  --{}-->  {}\n", short(&r.from), r.rtype, short(&r.to)));
    }
    s
}

pub fn render_audit(store: &Store) -> String {
    let mut s = String::from("Audit log (immutable events):\n");
    for ev in store.events() {
        s.push_str(&format!("  [{}] {} by {}\n", short(&ev.correlation_id), ev.etype, ev.actor));
    }
    s
}

pub fn render_event(ev: &EventRecord) -> String {
    format!("{} {} {}", ev.etype, ev.actor, ev.payload)
}

fn short(id: &str) -> String {
    if id.len() > 8 { id[id.len() - 8..].to_string() } else { id.to_string() }
}
fn truncate(s: &str, n: usize) -> String {
    if s.len() > n { format!("{}...", &s[..n]) } else { s.to_string() }
}
