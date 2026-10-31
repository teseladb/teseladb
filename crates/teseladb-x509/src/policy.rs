//! Certificate policy processing of RFC 5280 (6.1.2 to 6.1.5): the `valid_policy_tree`, the
//! `explicit_policy`, `policy_mapping` and `inhibit_anyPolicy` counters and the intersection
//! with the user's initial policy set. Qualifiers are not processed.

use crate::Error;
use crate::cert::Certificate;
use crate::oid;

/// The policy inputs of path validation (RFC 5280, 6.1.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicySettings {
    /// `user-initial-policy-set`; `None` is any-policy.
    pub initial_policies: Option<Vec<Vec<u8>>>,
    /// `initial-explicit-policy`.
    pub explicit: bool,
    /// `initial-policy-mapping-inhibit`.
    pub inhibit_mapping: bool,
    /// `initial-any-policy-inhibit`.
    pub inhibit_any: bool,
}

#[derive(Clone, Debug)]
struct Node {
    policy: Vec<u8>,
    expected: Vec<Vec<u8>>,
    parent: Option<usize>,
    depth: usize,
    alive: bool,
}

struct Tree {
    nodes: Vec<Node>,
    null: bool,
}

impl Tree {
    fn at(&self, depth: usize) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| self.nodes[i].alive && self.nodes[i].depth == depth)
            .collect()
    }

    fn add(&mut self, parent: usize, policy: &[u8], expected: Vec<Vec<u8>>) {
        let depth = self.nodes[parent].depth + 1;
        self.nodes.push(Node {
            policy: policy.to_vec(),
            expected,
            parent: Some(parent),
            depth,
            alive: true,
        });
    }

    fn has_child(&self, parent: usize) -> bool {
        self.nodes.iter().any(|n| n.alive && n.parent == Some(parent))
    }

    fn kill(&mut self, i: usize) {
        self.nodes[i].alive = false;
        let children: Vec<usize> = (0..self.nodes.len())
            .filter(|&c| self.nodes[c].alive && self.nodes[c].parent == Some(i))
            .collect();
        for c in children {
            self.kill(c);
        }
    }

    /// Removes nodes above `depth` that have no children, bottom-up, and marks the tree NULL if
    /// nothing is left.
    fn prune(&mut self, depth: usize) {
        for d in (0..depth).rev() {
            for i in self.at(d) {
                if !self.has_child(i) {
                    self.nodes[i].alive = false;
                }
            }
        }
        if self.nodes.iter().all(|n| !n.alive) {
            self.null = true;
        }
    }
}

fn dec(v: &mut u64) {
    *v = v.saturating_sub(1);
}

/// Runs policy processing over `path` (trust anchor excluded, the certificate it issued first).
pub(crate) fn process(path: &[&Certificate], settings: &PolicySettings) -> Result<(), Error> {
    let n = path.len() as u64;
    let start = |inhibited: bool| if inhibited { 0 } else { n + 1 };
    let mut explicit = start(settings.explicit);
    let mut inhibit_any = start(settings.inhibit_any);
    let mut mapping = start(settings.inhibit_mapping);
    let mut tree = Tree {
        nodes: vec![Node {
            policy: oid::ANY_POLICY.to_vec(),
            expected: vec![oid::ANY_POLICY.to_vec()],
            parent: None,
            depth: 0,
            alive: true,
        }],
        null: false,
    };
    for (idx, cert) in path.iter().enumerate() {
        let i = idx + 1;
        let last = i == path.len();
        let self_issued = cert.is_self_issued();
        // 6.1.3 (d) and (e).
        match (&cert.policies, tree.null) {
            (Some(policies), false) => {
                let parents = tree.at(i - 1);
                for p in policies.iter().filter(|p| p.as_slice() != oid::ANY_POLICY) {
                    let matched: Vec<usize> = parents
                        .iter()
                        .copied()
                        .filter(|&q| tree.nodes[q].expected.contains(p))
                        .collect();
                    let targets = if matched.is_empty() {
                        parents
                            .iter()
                            .copied()
                            .filter(|&q| tree.nodes[q].policy == oid::ANY_POLICY)
                            .collect()
                    } else {
                        matched
                    };
                    for q in targets {
                        tree.add(q, p, vec![p.clone()]);
                    }
                }
                let has_any = policies.iter().any(|p| p.as_slice() == oid::ANY_POLICY);
                if has_any && (inhibit_any > 0 || (!last && self_issued)) {
                    for &q in &parents {
                        for e in tree.nodes[q].expected.clone() {
                            let exists = tree
                                .nodes
                                .iter()
                                .any(|c| c.alive && c.parent == Some(q) && c.policy == e);
                            if !exists {
                                tree.add(q, &e, vec![e.clone()]);
                            }
                        }
                    }
                }
                tree.prune(i);
                if tree.at(i).is_empty() {
                    tree.null = true;
                }
            }
            _ => tree.null = true,
        }
        // 6.1.3 (f).
        if explicit == 0 && tree.null {
            return Err(Error::Policy);
        }
        if last {
            break;
        }
        // 6.1.4 (a) and (b).
        let any = oid::ANY_POLICY;
        if cert.policy_mappings.iter().any(|(a, b)| a == any || b == any) {
            return Err(Error::Policy);
        }
        if !tree.null && !cert.policy_mappings.is_empty() {
            let mut issuer_policies: Vec<&Vec<u8>> =
                cert.policy_mappings.iter().map(|(a, _)| a).collect();
            issuer_policies.dedup();
            for idp in issuer_policies {
                let mapped: Vec<Vec<u8>> = cert
                    .policy_mappings
                    .iter()
                    .filter(|(a, _)| a == idp)
                    .map(|(_, b)| b.clone())
                    .collect();
                let nodes: Vec<usize> =
                    tree.at(i).into_iter().filter(|&x| tree.nodes[x].policy == *idp).collect();
                if mapping > 0 {
                    if nodes.is_empty() {
                        let any_node =
                            tree.at(i).into_iter().find(|&x| tree.nodes[x].policy == any);
                        if let Some(parent) = any_node.and_then(|x| tree.nodes[x].parent) {
                            tree.add(parent, idp, mapped);
                        }
                    } else {
                        for x in nodes {
                            tree.nodes[x].expected = mapped.clone();
                        }
                    }
                } else {
                    for x in nodes {
                        tree.kill(x);
                    }
                    tree.prune(i);
                }
            }
        }
        // 6.1.4 (h), (i) and (j).
        if !self_issued {
            dec(&mut explicit);
            dec(&mut mapping);
            dec(&mut inhibit_any);
        }
        if let Some(r) = cert.require_explicit_policy {
            explicit = explicit.min(r);
        }
        if let Some(m) = cert.inhibit_policy_mapping {
            mapping = mapping.min(m);
        }
        if let Some(a) = cert.inhibit_any_policy {
            inhibit_any = inhibit_any.min(a);
        }
    }
    // 6.1.5 (a) and (b).
    dec(&mut explicit);
    if path.last().and_then(|c| c.require_explicit_policy) == Some(0) {
        explicit = 0;
    }
    // 6.1.5 (g).
    if let (Some(user), false) = (&settings.initial_policies, tree.null) {
        intersect(&mut tree, user, path.len());
    }
    if explicit > 0 || !tree.null { Ok(()) } else { Err(Error::Policy) }
}

fn intersect(tree: &mut Tree, user: &[Vec<u8>], n: usize) {
    let any = oid::ANY_POLICY;
    let node_set: Vec<usize> = (0..tree.nodes.len())
        .filter(|&x| {
            tree.nodes[x].alive && tree.nodes[x].parent.is_some_and(|p| tree.nodes[p].policy == any)
        })
        .collect();
    for &x in &node_set {
        let p = &tree.nodes[x].policy;
        if p != any && !user.contains(p) {
            tree.kill(x);
        }
    }
    let leaf_any = tree.at(n).into_iter().find(|&x| tree.nodes[x].policy == any);
    if let Some(leaf) = leaf_any {
        let parent = tree.nodes[leaf].parent;
        for u in user {
            let present =
                node_set.iter().any(|&x| tree.nodes[x].alive && tree.nodes[x].policy == *u);
            if let (false, Some(parent)) = (present, parent) {
                tree.add(parent, u, vec![u.clone()]);
            }
        }
        tree.kill(leaf);
    }
    tree.prune(n);
}
