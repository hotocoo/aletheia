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
    /// The argument names this operation expects. Declared HERE, next to the operation itself, so
    /// the prompt that tells a model what arguments to produce is generated from the same registry
    /// the validator checks against. Written anywhere else it would be a second list, and a second
    /// list is a list that drifts — silently, into the one place where drift looks like the model
    /// being wrong.
    pub args: &'static [&'static str],
}

pub fn lookup(op: &str) -> Option<OpMeta> {
    let m = match op {
        "entity.read" => OpMeta {
            name: "entity.read",
            risk: Risk::Safe,
            action: "entity.read",
            args: &["id"],
        },
        "entity.derive" => OpMeta {
            name: "entity.derive",
            risk: Risk::Safe,
            action: "entity.derive",
            args: &["source", "into_type", "content"],
        },
        "world.traverse" => OpMeta {
            name: "world.traverse",
            risk: Risk::Safe,
            action: "entity.read",
            args: &["from", "edge"],
        },
        "capability.grant" => OpMeta {
            name: "capability.grant",
            risk: Risk::Safe,
            action: "capability.grant",
            args: &["subject", "action", "scope_entities", "approval"],
        },
        "entity.restore_version" => OpMeta {
            name: "entity.restore_version",
            risk: Risk::Safe,
            action: "entity.write",
            args: &["chain", "version"],
        },
        "entity.delete" => OpMeta {
            name: "entity.delete",
            risk: Risk::Destructive,
            action: "entity.delete",
            args: &["id"],
        },
        _ => return None,
    };
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered operation declares its arguments. An operation with an empty list would
    /// produce a prompt line that tells the model nothing, and the resulting plan would fail
    /// downstream for a reason no one could trace back to here.
    #[test]
    fn every_operation_declares_its_arguments() {
        for op in [
            "entity.read",
            "entity.derive",
            "world.traverse",
            "capability.grant",
            "entity.restore_version",
            "entity.delete",
        ] {
            let m = lookup(op).expect("registered");
            assert!(!m.args.is_empty(), "{op} declares no arguments");
            assert_eq!(m.name, op);
        }
    }

    #[test]
    fn an_unregistered_operation_is_not_looked_up() {
        assert!(lookup("entity.wipe").is_none());
    }
}
