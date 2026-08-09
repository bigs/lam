use lam::InterruptionReceipt;
use serde::Serialize;

use crate::ActorAddress;

/// Durable result of interrupting one member of an agent tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterruptionReceipt {
    /// Canonical address of the affected actor.
    pub address: ActorAddress,
    /// Durable run boundary, absent when that actor had no active run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interruption: Option<InterruptionReceipt>,
}

/// How far a recoverable interruption reaches from its addressed actor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InterruptionScope {
    /// Interrupt only the addressed actor's active run. Resident descendants
    /// keep running and their outcomes remain deliverable.
    Actor,
    /// Interrupt the addressed actor and every resident descendant, retiring
    /// the descendants after they commit their interruption boundaries.
    #[default]
    Subtree,
}

/// Result of recoverably interrupting a root and retiring its descendants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTreeInterruptionReceipt {
    /// Root of the interrupted tree. This actor remains resident.
    pub root: ActorAddress,
    /// Deterministically ordered interruption result for every resident actor.
    pub actors: Vec<AgentInterruptionReceipt>,
}
