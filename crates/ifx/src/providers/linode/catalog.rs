//! Known Linode regions, plan types and public images, snapshotted from the public API
//! into `catalog.json` (refresh with `cargo run -p ifx-gen -- --fetch-catalog`). These
//! feed open enums: known values get names and completion, new ones still validate.

use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub regions: Vec<Entry>,
    pub types: Vec<Entry>,
    pub images: Vec<Entry>,
}

pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("catalog.json")).expect("catalog.json is valid")
    })
}

pub fn regions() -> Vec<String> {
    catalog().regions.iter().map(|e| e.id.clone()).collect()
}

pub fn types() -> Vec<String> {
    catalog().types.iter().map(|e| e.id.clone()).collect()
}

pub fn images() -> Vec<String> {
    catalog().images.iter().map(|e| e.id.clone()).collect()
}
