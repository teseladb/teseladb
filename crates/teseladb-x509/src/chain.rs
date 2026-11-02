//! Certification path building and validation (RFC 5280, section 6): signatures, validity,
//! name chaining, basic constraints and path length, key usage, extended key usage, name
//! constraints, policies, unknown critical extensions and revocation with CRLs (6.3), and the
//! host name check of a server certificate (RFC 6125).
//!
//! The trust anchor's own `pathLenConstraint`, `keyUsage` and `nameConstraints` apply to the
//! path and its validity dates are checked, as OpenSSL (and so RefDb) does.

use crate::Error;
use crate::cert::Certificate;
use crate::crl::{Crl, REASON_REMOVE_FROM_CRL};
use crate::ext::{DistributionPoint, GeneralName, key_usage, reasons};
use crate::key::{PublicKey, SignatureAlgorithm};
use crate::oid;
use crate::policy::{self, PolicySettings};
use teseladb_crypto::HashAlgorithm;

/// Default longest path, certificates included.
pub const DEFAULT_MAX_DEPTH: usize = 32;
/// How deep the search for a CRL signer's own path may recurse.
const MAX_CRL_NESTING: usize = 3;

/// What the leaf certificate will be used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// No extended key usage requirement.
    Any,
    /// TLS server (`id-kp-serverAuth`).
    ServerAuth,
    /// TLS client (`id-kp-clientAuth`).
    ClientAuth,
}

/// Which certificates of the path need a CRL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrlCheck {
    /// No revocation checking.
    None,
    /// Only the leaf (OpenSSL's `X509_V_FLAG_CRL_CHECK`, what `--tlsCRLFile` turns on).
    Leaf,
    /// Every certificate below the trust anchor.
    All,
}

/// Inputs of path validation.
#[derive(Clone, Debug)]
pub struct VerifyOptions<'a> {
    /// Validation time, seconds since the epoch.
    pub now: i64,
    /// Intended use of the leaf.
    pub purpose: Purpose,
    /// Available CRLs, complete and delta.
    pub crls: &'a [Crl],
    /// Which certificates need a CRL.
    pub crl_check: CrlCheck,
    /// Policy inputs.
    pub policy: PolicySettings,
    /// Longest path, certificates included.
    pub max_depth: usize,
    /// Whether signatures with SHA-1 are accepted. OpenSSL 3, which RefDb 9.0.4 uses, refuses
    /// them from security level 1 on (measured); old test suites such as PKITS need them.
    pub weak_digests: bool,
}

impl<'a> VerifyOptions<'a> {
    /// Defaults: any purpose, no revocation checking, any policy.
    pub fn new(now: i64) -> VerifyOptions<'a> {
        VerifyOptions {
            now,
            purpose: Purpose::Any,
            crls: &[],
            crl_check: CrlCheck::None,
            policy: PolicySettings::default(),
            max_depth: DEFAULT_MAX_DEPTH,
            weak_digests: false,
        }
    }
}

struct Ctx<'a> {
    intermediates: &'a [Certificate],
    anchors: &'a [Certificate],
    opts: &'a VerifyOptions<'a>,
    /// CRL signers whose own path is being validated further up: they may sign the CRL that
    /// covers themselves without another level of recursion.
    pending: Vec<&'a Certificate>,
}

fn same(a: &Certificate, b: &Certificate) -> bool {
    a.der() == b.der()
}

/// Whether `parent` may have issued `child`: names chain and the key identifiers, when both
/// are present, agree.
fn may_issue(parent: &Certificate, child: &Certificate) -> bool {
    parent.subject == child.issuer
        && match (&child.authority_key_id, &parent.subject_key_id) {
            (Some(a), Some(s)) => a == s,
            _ => true,
        }
}

/// Refuses signature algorithm `alg` when its digest is too weak and `opts` does not allow it.
fn check_digest(alg: SignatureAlgorithm, opts: &VerifyOptions<'_>) -> Result<(), Error> {
    let hash = match alg {
        SignatureAlgorithm::RsaPkcs1(h) | SignatureAlgorithm::Ecdsa(h) => Some(h),
        SignatureAlgorithm::RsaPss { hash, .. } => Some(hash),
        SignatureAlgorithm::Ed25519 => None,
    };
    if hash == Some(HashAlgorithm::Sha1) && !opts.weak_digests {
        return Err(Error::WeakDigest);
    }
    Ok(())
}

/// Validates `leaf` up to one of `anchors`, using `intermediates` to build the path. Returns the
/// path from the leaf to the anchor.
pub fn verify_chain<'a>(
    leaf: &'a Certificate,
    intermediates: &'a [Certificate],
    anchors: &'a [Certificate],
    opts: &'a VerifyOptions<'a>,
) -> Result<Vec<&'a Certificate>, Error> {
    let ctx = Ctx { intermediates, anchors, opts, pending: Vec::new() };
    build(&ctx, leaf, 0)
}

fn build<'a>(
    ctx: &Ctx<'a>,
    leaf: &'a Certificate,
    nesting: usize,
) -> Result<Vec<&'a Certificate>, Error> {
    if let Some(anchor) = ctx.anchors.iter().find(|a| same(a, leaf)) {
        check_dates(anchor, ctx.opts.now)?;
        check_purpose(anchor, ctx.opts.purpose)?;
        return Ok(vec![anchor]);
    }
    let mut chain = vec![leaf];
    let mut err = None;
    match dfs(ctx, &mut chain, &mut err, nesting) {
        Some(path) => Ok(path),
        None => Err(err.unwrap_or(Error::UnknownIssuer)),
    }
}

fn dfs<'a>(
    ctx: &Ctx<'a>,
    chain: &mut Vec<&'a Certificate>,
    err: &mut Option<Error>,
    nesting: usize,
) -> Option<Vec<&'a Certificate>> {
    let cur = *chain.last()?;
    if chain.len() >= ctx.opts.max_depth {
        err.get_or_insert(Error::PathLength);
        return None;
    }
    for anchor in ctx.anchors.iter().filter(|a| may_issue(a, cur)) {
        let mut path = chain.clone();
        path.push(anchor);
        match validate(ctx, &path, nesting) {
            Ok(()) => return Some(path),
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    for cand in ctx.intermediates.iter().filter(|c| may_issue(c, cur)) {
        if chain.iter().any(|c| same(c, cand)) {
            continue;
        }
        chain.push(cand);
        if let Some(p) = dfs(ctx, chain, err, nesting) {
            return Some(p);
        }
        chain.pop();
    }
    None
}

fn check_dates(c: &Certificate, now: i64) -> Result<(), Error> {
    if now < c.not_before {
        return Err(Error::NotYetValid);
    }
    if now > c.not_after {
        return Err(Error::Expired);
    }
    Ok(())
}

fn check_purpose(c: &Certificate, purpose: Purpose) -> Result<(), Error> {
    let wanted = match purpose {
        Purpose::Any => return Ok(()),
        Purpose::ServerAuth => oid::SERVER_AUTH,
        Purpose::ClientAuth => oid::CLIENT_AUTH,
    };
    match &c.ext_key_usage {
        Some(ekus) if !ekus.iter().any(|e| e == wanted || e == oid::ANY_EXTENDED_KEY_USAGE) => {
            Err(Error::ExtKeyUsage)
        }
        _ => Ok(()),
    }
}

/// Accumulated name constraints: every permitted set must allow a name, no excluded subtree may
/// contain it.
#[derive(Default)]
struct Constraints {
    permitted: Vec<Vec<GeneralName>>,
    excluded: Vec<GeneralName>,
}

impl Constraints {
    fn add(&mut self, c: &Certificate) {
        if let Some(nc) = &c.name_constraints {
            if let Some(p) = &nc.permitted {
                self.permitted.push(p.clone());
            }
            self.excluded.extend(nc.excluded.iter().cloned());
        }
    }

    fn check(&self, name: &GeneralName) -> Result<(), Error> {
        if self.excluded.iter().any(|c| same_type(c, name) && within(name, c)) {
            return Err(Error::NameConstraints);
        }
        for set in &self.permitted {
            let relevant: Vec<&GeneralName> = set.iter().filter(|c| same_type(c, name)).collect();
            if !relevant.is_empty() && !relevant.iter().any(|c| within(name, c)) {
                return Err(Error::NameConstraints);
            }
        }
        Ok(())
    }

    fn check_cert(&self, c: &Certificate) -> Result<(), Error> {
        if !c.subject.is_empty() {
            self.check(&GeneralName::Directory(c.subject.clone()))?;
        }
        if !c.subject_alt_names.iter().any(|g| matches!(g, GeneralName::Email(_))) {
            for email in c.subject.values(oid::EMAIL_ADDRESS) {
                self.check(&GeneralName::Email(email))?;
            }
        }
        c.subject_alt_names.iter().try_for_each(|g| self.check(g))
    }
}

fn same_type(a: &GeneralName, b: &GeneralName) -> bool {
    match (a, b) {
        (GeneralName::Other(x, _), GeneralName::Other(y, _)) => x == y,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

fn dns_within(name: &str, constraint: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let c = constraint.trim_end_matches('.').to_ascii_lowercase();
    if c.is_empty() {
        return true;
    }
    match c.strip_prefix('.') {
        Some(_) => name.ends_with(&c),
        None => name == c || name.ends_with(&format!(".{c}")),
    }
}

fn host_within(host: &str, constraint: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let c = constraint.to_ascii_lowercase();
    if c.starts_with('.') { host.ends_with(&c) } else { host == c }
}

fn email_within(name: &str, constraint: &str) -> bool {
    let Some((local, host)) = name.rsplit_once('@') else {
        return false;
    };
    match constraint.rsplit_once('@') {
        Some((cl, ch)) => local == cl && host.eq_ignore_ascii_case(ch),
        None => host_within(host, constraint),
    }
}

fn uri_host(uri: &str) -> Option<&str> {
    let rest = uri.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host.split(':').next()?,
    };
    (!host.is_empty()).then_some(host)
}

fn within(name: &GeneralName, constraint: &GeneralName) -> bool {
    match (name, constraint) {
        (GeneralName::Dns(n), GeneralName::Dns(c)) => dns_within(n, c),
        (GeneralName::Email(n), GeneralName::Email(c)) => email_within(n, c),
        (GeneralName::Uri(n), GeneralName::Uri(c)) => {
            uri_host(n).is_some_and(|h| host_within(h, c))
        }
        (GeneralName::Directory(n), GeneralName::Directory(c)) => n.starts_with(c),
        (GeneralName::Ip(n), GeneralName::Ip(c)) => {
            c.len() == 2 * n.len()
                && n.iter().zip(&c[..n.len()]).zip(&c[n.len()..]).all(|((a, b), m)| a & m == b & m)
        }
        // Forms this crate cannot compare: a constraint on them is never satisfied.
        _ => false,
    }
}

fn validate(ctx: &Ctx<'_>, path: &[&Certificate], nesting: usize) -> Result<(), Error> {
    let opts = ctx.opts;
    let anchor = path[path.len() - 1];
    // From the certificate the anchor issued down to the leaf.
    let certs: Vec<&Certificate> = path[..path.len() - 1].iter().rev().copied().collect();
    let n = certs.len();
    check_dates(anchor, opts.now)?;
    if anchor.version == 3 && !anchor.is_ca() {
        return Err(Error::NotCa);
    }
    if anchor.key_usage.is_some_and(|k| k & key_usage::KEY_CERT_SIGN == 0) {
        return Err(Error::KeyUsage);
    }
    let mut constraints = Constraints::default();
    constraints.add(anchor);
    let mut max_path = n as u64;
    if let Some(p) = anchor.basic_constraints.and_then(|b| b.path_len) {
        max_path = max_path.min(p);
    }
    let mut working_key: &PublicKey = &anchor.public_key;
    let mut issuer: &Certificate = anchor;
    for (idx, c) in certs.iter().enumerate() {
        let last = idx + 1 == n;
        check_digest(c.signature_algorithm, opts)?;
        c.verify_signed_by(working_key)?;
        check_dates(c, opts.now)?;
        let needs_crl = match opts.crl_check {
            CrlCheck::None => false,
            CrlCheck::Leaf => last,
            CrlCheck::All => true,
        };
        if needs_crl {
            check_revocation(ctx, c, issuer, anchor, nesting)?;
        }
        if last || !c.is_self_issued() {
            constraints.check_cert(c)?;
        }
        if c.unknown_critical {
            return Err(Error::UnknownCriticalExtension);
        }
        if !last {
            if !c.is_ca() {
                return Err(Error::NotCa);
            }
            if !c.is_self_issued() {
                if max_path == 0 {
                    return Err(Error::PathLength);
                }
                max_path -= 1;
            }
            if let Some(p) = c.basic_constraints.and_then(|b| b.path_len) {
                max_path = max_path.min(p);
            }
            if c.key_usage.is_some_and(|k| k & key_usage::KEY_CERT_SIGN == 0) {
                return Err(Error::KeyUsage);
            }
            constraints.add(c);
            check_purpose(c, opts.purpose)?;
        }
        working_key = &c.public_key;
        issuer = c;
    }
    policy::process(&certs, &opts.policy)?;
    certs.last().map_or(Ok(()), |leaf| check_purpose(leaf, opts.purpose))
}

/// The key that signed `crl` as RFC 5280, 6.3.3 (f) asks: the issuer of the certificate if it
/// may sign CRLs, otherwise a certificate of the CRL issuer whose own path validates to the same
/// trust anchor.
fn crl_signers<'a>(
    ctx: &Ctx<'a>,
    crl: &Crl,
    issuer: &'a Certificate,
    anchor: &Certificate,
    nesting: usize,
) -> Vec<&'a Certificate> {
    let can_sign = |c: &Certificate| {
        c.subject == crl.issuer
            && c.key_usage.is_none_or(|k| k & key_usage::CRL_SIGN != 0)
            && match (&crl.authority_key_id, &c.subject_key_id) {
                (Some(a), Some(s)) => a == s,
                _ => true,
            }
    };
    let mut out = Vec::new();
    if can_sign(issuer) {
        out.push(issuer);
    }
    out.extend(ctx.pending.iter().copied().filter(|c| can_sign(c) && !same(c, issuer)));
    if nesting >= MAX_CRL_NESTING {
        return out;
    }
    let pool = ctx.intermediates.iter().chain(ctx.anchors.iter());
    let fresh = |c: &&Certificate| !same(c, issuer) && !ctx.pending.iter().any(|p| same(p, c));
    for cand in pool.filter(|c| can_sign(c)).filter(fresh) {
        let sub = VerifyOptions { purpose: Purpose::Any, ..ctx.opts.clone() };
        let mut pending = ctx.pending.clone();
        pending.push(cand);
        let sub_ctx = Ctx {
            intermediates: ctx.intermediates,
            anchors: std::slice::from_ref(anchor),
            opts: &sub,
            pending,
        };
        if build(&sub_ctx, cand, nesting + 1).is_ok() {
            out.push(cand);
        }
    }
    out
}

fn check_revocation(
    ctx: &Ctx<'_>,
    cert: &Certificate,
    issuer: &Certificate,
    anchor: &Certificate,
    nesting: usize,
) -> Result<(), Error> {
    let now = ctx.opts.now;
    let default_dp = [DistributionPoint { name: None, reasons: None, crl_issuer: Vec::new() }];
    let dps: &[DistributionPoint] = if cert.crl_distribution_points.is_empty() {
        &default_dp
    } else {
        &cert.crl_distribution_points
    };
    let usable = |c: &&Crl| {
        !c.unknown_critical
            && c.is_current(now)
            && check_digest(c.signature_algorithm, ctx.opts).is_ok()
    };
    let mut mask = 0u16;
    let mut revoked = false;
    for dp in dps {
        for crl in ctx.opts.crls.iter().filter(usable).filter(|c| c.delta_base.is_none()) {
            if mask & reasons::ALL == reasons::ALL || revoked {
                break;
            }
            if !crl.in_scope(cert, dp)? {
                continue;
            }
            let interim = crl.reasons() & dp.reasons.unwrap_or(reasons::ALL);
            if interim & !mask == 0 {
                continue;
            }
            let signers = crl_signers(ctx, crl, issuer, anchor, nesting);
            let Some(signer) = signers.iter().find(|s| {
                s.public_key.verify(crl.signature_algorithm, crl.tbs(), &crl.signature).is_ok()
            }) else {
                continue;
            };
            let delta_entry = ctx
                .opts
                .crls
                .iter()
                .filter(usable)
                .filter(|d| crl.accepts_delta(d))
                .filter(|d| {
                    signer.public_key.verify(d.signature_algorithm, d.tbs(), &d.signature).is_ok()
                })
                .find_map(|d| d.find(cert));
            let entry = delta_entry.or_else(|| crl.find(cert));
            if entry.is_some_and(|e| e.reason != Some(REASON_REMOVE_FROM_CRL)) {
                revoked = true;
            }
            mask |= interim;
        }
    }
    if revoked {
        Err(Error::Revoked)
    } else if mask & reasons::ALL == reasons::ALL {
        Ok(())
    } else {
        Err(Error::RevocationUnknown)
    }
}

/// Checks that `cert` is valid for `host` (RFC 6125): an IP address literal against the
/// iPAddress names, otherwise against the dNSName names with a wildcard allowed only as the whole
/// leftmost label. The common name is not consulted.
pub fn verify_hostname(cert: &Certificate, host: &str) -> Result<(), Error> {
    let host = host.trim_end_matches('.');
    if let Some(bytes) = crate::ip::parse(host) {
        return if cert.subject_alt_names.contains(&GeneralName::Ip(bytes)) {
            Ok(())
        } else {
            Err(Error::HostnameMismatch)
        };
    }
    let ok = cert.subject_alt_names.iter().any(|g| match g {
        GeneralName::Dns(pattern) => dns_matches(pattern, host),
        _ => false,
    });
    if ok { Ok(()) } else { Err(Error::HostnameMismatch) }
}

fn dns_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim_end_matches('.');
    if host.is_empty() || pattern.is_empty() {
        return false;
    }
    match pattern.strip_prefix("*.") {
        Some(rest) => {
            // The wildcard covers exactly one label and never a bare public suffix.
            rest.contains('.')
                && host.split_once('.').is_some_and(|(label, tail)| {
                    !label.is_empty() && tail.eq_ignore_ascii_case(rest)
                })
        }
        None => !pattern.contains('*') && pattern.eq_ignore_ascii_case(host),
    }
}
