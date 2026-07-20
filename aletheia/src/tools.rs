//! Operation registry (PRD-002 §22 SDK-003, SAD §15). Each op declares its risk and the capability
//! action it requires. Executors/verifiers are implemented in syscore (which holds the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Safe,
    Destructive,
}

#[derive(Debug, Clone, Copy)]
pub struct OpMeta {
    pub name: &'static str,
    pub risk: Risk,
    pub action: &'static str,
}

pub fn lookup(op: &str) -> Option<OpMeta> {
    let m = match op {
        "entity.read" => OpMeta { name: "entity.read", risk: Risk::Safe, action: "entity.read" },
        "entity.derive" => OpMeta { name: "entity.derive", risk: Risk::Safe, action: "entity.derive" },
        "world.traverse" => OpMeta { name: "world.traverse", risk: Risk::Safe, action: "entity.read" },
        "capability.grant" => OpMeta { name: "capability.grant", risk: Risk::Safe, action: "capability.grant" },
        "entity.restore_version" => OpMeta { name: "entity.restore_version", risk: Risk::Safe, action: "entity.write" },
        "entity.delete" => OpMeta { name: "entity.delete", risk: Risk::Destructive, action: "entity.delete" },
        _ => return None,
    };
    Some(m)
}
