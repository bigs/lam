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

/// Result of recoverably interrupting a root and retiring its descendants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTreeInterruptionReceipt {
    /// Root of the interrupted tree. This actor remains resident.
    pub root: ActorAddress,
    /// Deterministically ordered interruption result for every resident actor.
    pub actors: Vec<AgentInterruptionReceipt>,
}
