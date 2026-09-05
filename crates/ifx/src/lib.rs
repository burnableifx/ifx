//! # ifx
//!
//! Infrastructure as one reconciled graph. Cloud resources (a Linode instance) and the
//! configuration inside them (files, packages, services) are the same kind of node;
//! the engine observes the world, computes a plan, and applies it in dependency order.
//!
//! Rust stacks produce a [`model::Program`] through the lightweight `ifx-program`
//! authoring crate. `ifxd` compiles and executes those stacks before reconciling them.
//!
//! Every resource type publishes a [`schema::ResourceSchema`], which drives
//! validation, plan rendering, the LSP, and generated stubs.

pub mod cli;
pub mod client;
pub mod color;
pub mod control;
pub mod engine;
pub mod explorer;
pub use ifx_program::generated;
pub mod graph;
pub use ifx_program::model;
pub mod monitor;
pub mod provider;
pub mod providers;
pub mod render;
pub mod rust;
pub mod schema;
pub use ifx_program::stack;
pub mod state;
pub mod store;
pub mod stubs;
pub mod transport;

pub use cli::LoadCtx;
pub use client::DaemonClient;
pub use control::{
    API_VERSION, ApprovalGrant, ApprovalRequirement, BuildPhase, ExecutionEvent, ExecutionRun,
    ExecutionStatus, LeaseExtension, LeaseRequest, ProgramResolveRequest, ProgramRevision,
    ProgramSubmission, RetryPolicy, RunRequest, StackBuildStatus, StackLease,
};
pub use engine::{Action, Engine, Options, Plan, Report};
pub use ifx_program::{
    Connection, Declare, Handle, Input, Program, ResourceDecl, ResourceType, Stack, Urn, secret,
};
pub use provider::{Checker, Handler, OperationKind, OperationRisk, Registry};
pub use state::State;
pub use store::{Health, Store};
