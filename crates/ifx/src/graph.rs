//! Dependency graph over declared resources: validation (dangling refs, cycles) and
//! topological ordering.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{ModelError, Program, Urn};

#[derive(Debug, Clone)]
pub struct Graph {
    /// urn -> direct dependencies
    deps: BTreeMap<Urn, BTreeSet<Urn>>,
    /// urn -> direct dependents
    rdeps: BTreeMap<Urn, BTreeSet<Urn>>,
    order: Vec<Urn>,
}

impl Graph {
    pub fn build(program: &Program) -> Result<Self, ModelError> {
        let mut deps: BTreeMap<Urn, BTreeSet<Urn>> = BTreeMap::new();
        for r in &program.resources {
            if deps.insert(r.urn.clone(), r.dependencies()).is_some() {
                return Err(ModelError::Duplicate(r.urn.clone()));
            }
        }
        for (urn, ds) in &deps {
            for d in ds {
                if !deps.contains_key(d) {
                    return Err(ModelError::DanglingRef(urn.clone(), d.clone()));
                }
            }
        }
        Self::from_deps(deps)
    }

    /// Build from an explicit dependency map (used for deletions from state).
    pub fn from_deps(deps: BTreeMap<Urn, BTreeSet<Urn>>) -> Result<Self, ModelError> {
        let mut rdeps: BTreeMap<Urn, BTreeSet<Urn>> =
            deps.keys().map(|k| (k.clone(), BTreeSet::new())).collect();
        for (urn, ds) in &deps {
            for d in ds {
                if let Some(set) = rdeps.get_mut(d) {
                    set.insert(urn.clone());
                }
            }
        }
        let order = topo(&deps)?;
        Ok(Self { deps, rdeps, order })
    }

    /// Dependencies before dependents; deterministic (lexical among ready nodes).
    pub fn order(&self) -> &[Urn] {
        &self.order
    }

    pub fn deps(&self, urn: &Urn) -> impl Iterator<Item = &Urn> {
        self.deps.get(urn).into_iter().flatten()
    }

    pub fn dependents(&self, urn: &Urn) -> impl Iterator<Item = &Urn> {
        self.rdeps.get(urn).into_iter().flatten()
    }

    pub fn contains(&self, urn: &Urn) -> bool {
        self.deps.contains_key(urn)
    }

    /// Transitive closure of dependents (excluding `urn`).
    pub fn all_dependents(&self, urn: &Urn) -> BTreeSet<Urn> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&Urn> = self.dependents(urn).collect();
        while let Some(u) = stack.pop() {
            if seen.insert(u.clone()) {
                stack.extend(self.dependents(u));
            }
        }
        seen
    }

    /// Transitive closure of dependencies (excluding `urn`).
    pub fn all_deps(&self, urn: &Urn) -> BTreeSet<Urn> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&Urn> = self.deps(urn).collect();
        while let Some(u) = stack.pop() {
            if seen.insert(u.clone()) {
                stack.extend(self.deps(u));
            }
        }
        seen
    }

    /// Graphviz rendering.
    pub fn to_dot(&self) -> String {
        let mut s = String::from("digraph ifx {\n  rankdir=LR;\n  node [shape=box];\n");
        for urn in &self.order {
            s.push_str(&format!("  \"{urn}\";\n"));
            for d in self.deps(urn) {
                s.push_str(&format!("  \"{d}\" -> \"{urn}\";\n"));
            }
        }
        s.push_str("}\n");
        s
    }
}

fn topo(deps: &BTreeMap<Urn, BTreeSet<Urn>>) -> Result<Vec<Urn>, ModelError> {
    let mut indeg: BTreeMap<&Urn, usize> = deps
        .iter()
        .map(|(k, v)| (k, v.iter().filter(|d| deps.contains_key(*d)).count()))
        .collect();
    let mut ready: BTreeSet<&Urn> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(u, _)| *u)
        .collect();
    let mut out = Vec::with_capacity(deps.len());
    while let Some(u) = ready.pop_first() {
        out.push(u.clone());
        for (v, ds) in deps {
            if ds.contains(u) {
                let n = indeg.get_mut(v).expect("present");
                *n -= 1;
                if *n == 0 {
                    ready.insert(v);
                }
            }
        }
    }
    if out.len() != deps.len() {
        let stuck = indeg
            .into_iter()
            .find(|(_, n)| *n > 0)
            .map(|(u, _)| u.clone())
            .expect("cycle");
        return Err(ModelError::Cycle(stuck));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputRef, ResourceDecl};
    use serde_json::json;

    fn decl(t: &str, n: &str, deps: &[&str]) -> ResourceDecl {
        let mut d = ResourceDecl::new(t, n, json!({}));
        d.depends_on = deps.iter().map(|s| Urn::parse(s).unwrap()).collect();
        d
    }

    #[test]
    fn orders_and_detects_cycles() {
        let p = Program {
            resources: vec![
                decl("t", "c", &["t:b"]),
                decl("t", "a", &[]),
                decl("t", "b", &["t:a"]),
            ],
        };
        let g = Graph::build(&p).unwrap();
        let o: Vec<&str> = g.order().iter().map(Urn::as_str).collect();
        assert_eq!(o, ["t:a", "t:b", "t:c"]);
        assert_eq!(g.all_dependents(&Urn::parse("t:a").unwrap()).len(), 2);

        let cyc = Program {
            resources: vec![decl("t", "a", &["t:b"]), decl("t", "b", &["t:a"])],
        };
        assert!(matches!(Graph::build(&cyc), Err(ModelError::Cycle(_))));
        let dangling = Program {
            resources: vec![decl("t", "a", &["t:zzz"])],
        };
        assert!(matches!(
            Graph::build(&dangling),
            Err(ModelError::DanglingRef(..))
        ));
    }

    #[test]
    fn refs_imply_edges() {
        let mut b = ResourceDecl::new("t", "b", json!({}));
        b.inputs = json!({"on": OutputRef::new(Urn::new("t", "a"), "conn").to_value()});
        let p = Program {
            resources: vec![b, decl("t", "a", &[])],
        };
        let g = Graph::build(&p).unwrap();
        assert_eq!(g.order()[0].as_str(), "t:a");
    }
}
