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

/// RFC 5280, 6.1.3 (d) and (e): the policies of the certificate at depth `i` grow the tree, and
/// `anyPolicy` stands for every expected policy when `any_allowed`.
fn add_certificate_policies(tree: &mut Tree, cert: &Certificate, i: usize, any_allowed: bool) {
    let Some(policies) = cert.policies.as_ref().filter(|_| !tree.null) else {
        tree.null = true;
        return;
    };
    let parents = tree.at(i - 1);
    for p in policies.iter().filter(|p| p.as_slice() != oid::ANY_POLICY) {
        let matched: Vec<usize> =
            parents.iter().copied().filter(|&q| tree.nodes[q].expected.contains(p)).collect();
        let targets = if matched.is_empty() {
            parents.iter().copied().filter(|&q| tree.nodes[q].policy == oid::ANY_POLICY).collect()
        } else {
            matched
        };
        for q in targets {
            tree.add(q, p, vec![p.clone()]);
        }
    }
    let has_any = policies.iter().any(|p| p.as_slice() == oid::ANY_POLICY);
    if has_any && any_allowed {
        for &q in &parents {
            for e in tree.nodes[q].expected.clone() {
                let exists =
                    tree.nodes.iter().any(|c| c.alive && c.parent == Some(q) && c.policy == e);
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

/// RFC 5280, 6.1.4 (a) and (b): the policy mappings of the certificate at depth `i`, applied to
/// the tree when `mapping_allowed` and otherwise deleting the mapped nodes. Each issuer domain
/// policy is processed once, however many mappings name it and in whatever order.
fn map_policies(
    tree: &mut Tree,
    cert: &Certificate,
    i: usize,
    mapping_allowed: bool,
) -> Result<(), Error> {
    let any = oid::ANY_POLICY;
    if cert.policy_mappings.iter().any(|(issuer, subject)| issuer == any || subject == any) {
        return Err(Error::Policy);
    }
    if tree.null {
        return Ok(());
    }
    let mut issuer_policies: Vec<&Vec<u8>> = Vec::new();
    for (issuer, _) in &cert.policy_mappings {
        if !issuer_policies.contains(&issuer) {
            issuer_policies.push(issuer);
        }
    }
    for idp in issuer_policies {
        let mapped: Vec<Vec<u8>> = cert
            .policy_mappings
            .iter()
            .filter(|(issuer, _)| issuer == idp)
            .map(|(_, subject)| subject.clone())
            .collect();
        let nodes: Vec<usize> =
            tree.at(i).into_iter().filter(|&x| tree.nodes[x].policy == *idp).collect();
        if !mapping_allowed {
            for x in nodes {
                tree.kill(x);
            }
            tree.prune(i);
        } else if nodes.is_empty() {
            let any_node = tree.at(i).into_iter().find(|&x| tree.nodes[x].policy == any);
            if let Some(parent) = any_node.and_then(|x| tree.nodes[x].parent) {
                tree.add(parent, idp, mapped);
            }
        } else {
            for x in nodes {
                tree.nodes[x].expected.clone_from(&mapped);
            }
        }
    }
    Ok(())
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
        let any_allowed = inhibit_any > 0 || (!last && self_issued);
        add_certificate_policies(&mut tree, cert, i, any_allowed);
        // 6.1.3 (f).
        if explicit == 0 && tree.null {
            return Err(Error::Policy);
        }
        if last {
            break;
        }
        // 6.1.4 (a) and (b).
        map_policies(&mut tree, cert, i, mapping > 0)?;
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_issuer_policy_named_by_mappings_that_are_not_adjacent_is_mapped_once() {
        let pem = include_str!("../tests/data/openssl/leaf.pem");
        let mut cert = Certificate::from_pem_all(pem).expect("the fixture parses").remove(0);
        let (mapped_policy, other_policy, first_target, other_target, second_target) =
            (vec![1], vec![2], vec![3], vec![4], vec![5]);
        cert.policies = Some(vec![oid::ANY_POLICY.to_vec()]);
        cert.policy_mappings = vec![
            (mapped_policy.clone(), first_target.clone()),
            (other_policy, other_target),
            (mapped_policy.clone(), second_target.clone()),
        ];
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
        add_certificate_policies(&mut tree, &cert, 1, true);
        map_policies(&mut tree, &cert, 1, true).expect("the mappings apply");
        let mapped: Vec<&Node> = tree
            .nodes
            .iter()
            .filter(|n| n.alive && n.depth == 1 && n.policy == mapped_policy)
            .collect();
        assert_eq!(mapped.len(), 1, "{:?}", tree.nodes);
        assert_eq!(mapped[0].expected, vec![first_target, second_target]);
    }
}
