//! Aletheia — AI-native operating system. M1 hosted System-Core reference implementation.
//!
//! Organized around the seven primitives (Entity, Capability, Context, Intent, Action, Memory,
//! Relationship) per PRD-002. Modules mirror the SAD crate boundaries; splitting into a cargo
//! workspace is a mechanical later step. Dependency direction points inward toward `domain`.
pub mod agents;
pub mod capabilities;
pub mod component;
pub mod context;
pub mod crypto;
pub mod domain;
pub mod experience;
pub mod intelligence;
pub mod intent_action;
pub mod memory;
pub mod policy;
pub mod storage;
pub mod syscore;
pub mod tools;
pub mod worldmodel;
