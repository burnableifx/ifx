//! Linode (Akamai) cloud resources via the v4 REST API.
//!
//! Handlers share one lazily-initialised [`Linode`] client. Credentials are read from
//! `CREDENTIALS_DIRECTORY/linode-token`, otherwise `LINODE_TOKEN`, on first request;
//! registering the provider never fails for stacks that do not use it.

pub mod api;
pub mod catalog;
mod domain;
mod domain_record;
mod firewall;
mod instance;

use std::sync::Arc;

pub use crate::generated::linode::*;
pub use api::{ApiError, Linode};
pub use domain::DomainHandler;
pub use domain_record::DomainRecordHandler;
pub use firewall::FirewallHandler;
pub use instance::InstanceHandler;

use crate::provider::Registry;

/// Register every `linode.*` handler against a client configured from the environment.
pub fn register(r: &mut Registry) {
    register_with(r, Arc::new(Linode::from_env()));
}

/// Register every `linode.*` handler against an explicit client (tests, custom endpoints).
pub fn register_with(r: &mut Registry, api: Arc<Linode>) {
    r.register(InstanceHandler::new(api.clone()));
    r.register(FirewallHandler::new(api.clone()));
    r.register(DomainHandler::new(api.clone()));
    r.register(DomainRecordHandler::new(api));
}
