//! Pure planner: turns a [`SandboxScope`] into Landlock path rules and a
//! network plan. No kernel calls, so it runs and is tested on any OS.
//!
//! Landlock is allow-only and a rule on a directory covers its whole subtree.
//! A deny inside an allowed tree is therefore expressed by walking the denied
//! path's ancestors: each ancestor directory gets no rule and each of its other
//! children gets the full right (masked to file rights for non-directories).
//!
//! Known limits:
//! - Ancestors of a denied path cannot be listed (their other children stay
//!   reachable by path).
//! - `stat` and traversal are not controlled by Landlock, so a denied path is
//!   indistinguishable from a missing one (unlike macOS).
//! - Files created in an ancestor after apply are not readable.
//! - Write denies inside the writable tree remove create/remove rights from
//!   the ancestors of the denied path.
//! - Glob deny patterns cannot be expressed; they are reported, not applied.

use std::path::{Path, PathBuf};

use crate::sandbox::{NetworkPolicy, SandboxScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rights {
    Read,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathRule {
    pub path: PathBuf,
    pub rights: Rights,
    /// `false` for non-directories: the applier must mask the rights to file
    /// rights (a directory right on a file fails with EINVAL).
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetPlan {
    pub handle_connect: bool,
    pub handle_bind: bool,
    pub connect_ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandlockPlan {
    pub read: Vec<PathRule>,
    pub write: Vec<PathRule>,
    pub net: NetPlan,
    pub skipped_globs: Vec<String>,
}

/// Canonicalize `p`; for a missing path canonicalize the nearest existing
/// ancestor and keep the tail. `None` when no ancestor resolves.
fn resolve(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        tail.push(cur.file_name()?.to_owned());
        cur = cur.parent()?;
        if let Ok(mut c) = cur.canonicalize() {
            c.extend(tail.iter().rev());
            return Some(c);
        }
    }
}

/// Emit rules for `path` and everything beneath it except the `denies`.
fn carve(path: &Path, denies: &[PathBuf], rights: Rights, out: &mut Vec<PathRule>) {
    if denies.iter().any(|d| path.starts_with(d)) {
        return;
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let is_dir = meta.is_dir();
    if !is_dir || !denies.iter().any(|d| d.starts_with(path)) {
        out.push(PathRule { path: path.to_path_buf(), rights, is_dir });
        return;
    }
    // A deny lies strictly beneath this directory: no rule for it, full right
    // for each child that is not (an ancestor of) a denied path.
    let Ok(rd) = std::fs::read_dir(path) else { return };
    let mut children: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        // Links are skipped: the kernel resolves real paths, so a link's target
        // is covered (or denied) through its own real path, and following one
        // could grant outside the declared scope or recurse forever on a cycle.
        if std::fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        carve(&child, denies, rights, out);
    }
}

fn carve_roots(roots: &[PathBuf], denies: &[PathBuf], rights: Rights) -> Vec<PathRule> {
    let mut out = Vec::new();
    for root in roots {
        if let Some(r) = resolve(root) {
            carve(&r, denies, rights, &mut out);
        }
    }
    out
}

pub(crate) fn plan(scope: &SandboxScope) -> LandlockPlan {
    let read_denies: Vec<PathBuf> = scope.read_deny.subpaths.iter().filter_map(|p| resolve(p)).collect();
    // Read denies are also carved out of the write rules, as Seatbelt denies both.
    let mut write_denies = read_denies.clone();
    write_denies.extend(scope.write_deny.subpaths.iter().filter_map(|p| resolve(p)));

    let read_roots =
        if scope.read_all { vec![PathBuf::from("/")] } else { scope.readable.clone() };
    let read = if scope.read_all && read_denies.is_empty() {
        vec![PathRule { path: PathBuf::from("/"), rights: Rights::Read, is_dir: true }]
    } else {
        carve_roots(&read_roots, &read_denies, Rights::Read)
    };
    let write = carve_roots(&scope.writable, &write_denies, Rights::ReadWrite);

    let (handle_connect, connect_ports) = match &scope.network {
        NetworkPolicy::Deny => (true, vec![]),
        NetworkPolicy::All => (false, vec![]),
        // Landlock filters by TCP port only; hosts are not enforced.
        NetworkPolicy::Hosts(_) => (true, vec![80, 443]),
    };
    let net = NetPlan { handle_connect, handle_bind: !scope.local_bind, connect_ports };

    let mut skipped_globs: Vec<String> = Vec::new();
    for g in scope.read_deny.globs.iter().chain(&scope.write_deny.globs) {
        if !skipped_globs.contains(g) {
            skipped_globs.push(g.clone());
        }
    }
    LandlockPlan { read, write, net, skipped_globs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{DenyPolicy, NetworkPolicy};
    use std::fs;
    use std::path::Path;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap();
        (d, p)
    }

    fn mk(p: &Path, file: bool) {
        if file {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        } else {
            fs::create_dir_all(p).unwrap();
        }
    }

    fn paths(rules: &[PathRule]) -> Vec<PathBuf> {
        rules.iter().map(|r| r.path.clone()).collect()
    }

    /// True when some rule covers `p` (rule path is `p` or an ancestor of it).
    fn covered(rules: &[PathRule], p: &Path) -> bool {
        rules.iter().any(|r| p.starts_with(&r.path))
    }

    fn deny(p: &Path) -> DenyPolicy {
        DenyPolicy { subpaths: vec![p.to_path_buf()], globs: vec![] }
    }

    #[test]
    fn read_all_without_deny_is_one_rule_on_root() {
        let scope = SandboxScope { read_all: true, ..Default::default() };
        let p = plan(&scope);
        assert_eq!(
            p.read,
            vec![PathRule { path: PathBuf::from("/"), rights: Rights::Read, is_dir: true }]
        );
        assert!(p.write.is_empty());
    }

    #[test]
    fn confined_reads_list_only_the_declared_paths() {
        let (_d, r) = tmp();
        mk(&r.join("a/f"), true);
        mk(&r.join("b/f"), true);
        mk(&r.join("c/f"), true);
        let scope = SandboxScope {
            readable: vec![r.join("a"), r.join("b")],
            ..Default::default()
        };
        let p = plan(&scope);
        assert_eq!(paths(&p.read), vec![r.join("a"), r.join("b")]);
        assert!(p.read.iter().all(|x| x.rights == Rights::Read && x.is_dir));
    }

    #[test]
    fn a_denied_subpath_under_read_all_is_not_granted_back() {
        let (_d, r) = tmp();
        mk(&r.join("home/me/.secret/key"), true);
        mk(&r.join("home/me/docs/a"), true);
        mk(&r.join("home/other/x"), true);
        mk(&r.join("etc/conf"), true);
        let denied = r.join("home/me/.secret");
        let scope = SandboxScope {
            readable: vec![r.clone()],
            writable: vec![r.join("home/me")],
            read_deny: deny(&denied),
            ..Default::default()
        };
        let p = plan(&scope);
        // Ancestors of the denied path get no rule.
        for anc in [r.clone(), r.join("home"), r.join("home/me")] {
            assert!(!paths(&p.read).contains(&anc), "ancestor {anc:?} has a read rule");
            assert!(!paths(&p.write).contains(&anc), "ancestor {anc:?} has a write rule");
        }
        // Siblings along the way get the full right.
        for sib in [r.join("etc"), r.join("home/other"), r.join("home/me/docs")] {
            let rule = p.read.iter().find(|x| x.path == sib).unwrap_or_else(|| panic!("{sib:?}"));
            assert_eq!(rule.rights, Rights::Read);
            assert!(rule.is_dir);
        }
        // Denied subtree appears in no rule, read or write.
        assert!(!covered(&p.read, &denied));
        assert!(!covered(&p.write, &denied));
        assert!(!covered(&p.read, &denied.join("key")));
        // Read-deny is carved out of the write rules too; siblings stay writable.
        assert!(paths(&p.write).contains(&r.join("home/me/docs")));
    }

    #[test]
    fn write_deny_removes_the_subtree_from_write_rules() {
        let (_d, r) = tmp();
        mk(&r.join("w/keep/f"), true);
        mk(&r.join("w/locked/f"), true);
        let scope = SandboxScope {
            readable: vec![r.join("w")],
            writable: vec![r.join("w")],
            write_deny: deny(&r.join("w/locked")),
            ..Default::default()
        };
        let p = plan(&scope);
        assert_eq!(paths(&p.write), vec![r.join("w/keep")]);
        assert_eq!(p.write[0].rights, Rights::ReadWrite);
        // Reads are unaffected by a write deny.
        assert_eq!(paths(&p.read), vec![r.join("w")]);
    }

    #[test]
    fn write_deny_inside_the_codebase_removes_create_rights_from_its_ancestors() {
        let (_d, r) = tmp();
        let code = r.join("code");
        mk(&code.join("src/main.rs"), true);
        mk(&code.join(".git/HEAD"), true);
        mk(&code.join("top.txt"), true);
        let scope = SandboxScope {
            writable: vec![code.clone()],
            write_deny: deny(&code.join(".git")),
            ..Default::default()
        };
        let p = plan(&scope);
        // The codebase root has no rule, so `touch <code>/new` fails: documented cost.
        assert!(!paths(&p.write).contains(&code));
        assert_eq!(paths(&p.write), vec![code.join("src"), code.join("top.txt")]);
        assert!(!covered(&p.write, &code.join("new")));
    }

    #[test]
    fn rights_are_masked_to_file_rights_for_non_directory_children() {
        let (_d, r) = tmp();
        mk(&r.join("d/sub/f"), true);
        mk(&r.join("d/file.txt"), true);
        mk(&r.join("d/secret/f"), true);
        let scope = SandboxScope {
            readable: vec![r.join("d")],
            read_deny: deny(&r.join("d/secret")),
            ..Default::default()
        };
        let p = plan(&scope);
        let file = p.read.iter().find(|x| x.path == r.join("d/file.txt")).unwrap();
        assert!(!file.is_dir);
        let dir = p.read.iter().find(|x| x.path == r.join("d/sub")).unwrap();
        assert!(dir.is_dir);
    }

    #[test]
    fn globs_are_reported_in_skipped_globs_not_applied() {
        let (_d, r) = tmp();
        mk(&r.join("d/f"), true);
        let scope = SandboxScope {
            readable: vec![r.join("d")],
            read_deny: DenyPolicy { subpaths: vec![], globs: vec!["**/.hidden*".into()] },
            write_deny: DenyPolicy { subpaths: vec![], globs: vec!["**/*.key".into()] },
            ..Default::default()
        };
        let p = plan(&scope);
        assert_eq!(p.skipped_globs, vec!["**/.hidden*".to_string(), "**/*.key".to_string()]);
        // Not applied: the read root stays a single rule.
        assert_eq!(paths(&p.read), vec![r.join("d")]);
    }

    #[test]
    fn nonexistent_deny_path_still_excluded() {
        let (_d, r) = tmp();
        mk(&r.join("d/real/f"), true);
        let missing = r.join("d/not/yet/here");
        let scope = SandboxScope {
            readable: vec![r.join("d")],
            read_deny: deny(&missing),
            ..Default::default()
        };
        let p = plan(&scope);
        assert!(!covered(&p.read, &missing));
        assert!(!paths(&p.read).contains(&r.join("d")));
        assert!(paths(&p.read).contains(&r.join("d/real")));
    }

    #[test]
    fn symlinked_deny_path_resolves_to_its_target() {
        let (_d, r) = tmp();
        mk(&r.join("d/target/f"), true);
        mk(&r.join("d/other/f"), true);
        std::os::unix::fs::symlink(r.join("d/target"), r.join("d/link")).unwrap();
        let scope = SandboxScope {
            readable: vec![r.join("d")],
            read_deny: deny(&r.join("d/link")),
            ..Default::default()
        };
        let p = plan(&scope);
        assert!(!covered(&p.read, &r.join("d/target")));
        assert!(!covered(&p.read, &r.join("d/target/f")));
        // The link itself resolves into the denied target, so it is not granted either.
        assert!(!p.read.iter().any(|x| x.path.starts_with(r.join("d/target"))));
        assert!(paths(&p.read).contains(&r.join("d/other")));
    }

    #[test]
    fn symlink_child_to_an_outside_dir_gets_no_rule() {
        let (_d, r) = tmp();
        mk(&r.join("work/.secret/k"), true);
        mk(&r.join("work/keep/f"), true);
        mk(&r.join("outside/f"), true);
        std::os::unix::fs::symlink(r.join("outside"), r.join("work/link")).unwrap();
        let scope = SandboxScope {
            readable: vec![r.join("work")],
            writable: vec![r.join("work")],
            read_deny: deny(&r.join("work/.secret")),
            ..Default::default()
        };
        let p = plan(&scope);
        for rules in [&p.read, &p.write] {
            assert!(!covered(rules, &r.join("outside")), "link target granted: {:?}", paths(rules));
            assert!(!paths(rules).contains(&r.join("work/link")), "{:?}", paths(rules));
        }
        assert!(paths(&p.write).contains(&r.join("work/keep")));
    }

    #[test]
    fn symlink_cycle_terminates() {
        let (_d, r) = tmp();
        mk(&r.join("home/me/.secret/k"), true);
        mk(&r.join("home/me/docs/f"), true);
        std::os::unix::fs::symlink(r.join("home"), r.join("home/me/loop")).unwrap();
        let scope = SandboxScope {
            readable: vec![r.join("home")],
            read_deny: deny(&r.join("home/me/.secret")),
            ..Default::default()
        };
        let p = plan(&scope);
        assert!(paths(&p.read).contains(&r.join("home/me/docs")));
        assert!(!covered(&p.read, &r.join("home/me/.secret")));
    }

    fn net_of(network: NetworkPolicy, local_bind: bool) -> NetPlan {
        plan(&SandboxScope { network, local_bind, ..Default::default() }).net
    }

    fn netp(c: bool, b: bool, ports: &[u16]) -> NetPlan {
        NetPlan { handle_connect: c, handle_bind: b, connect_ports: ports.to_vec() }
    }

    #[test]
    fn deny_handles_connect_and_bind_with_no_allow_rules() {
        assert_eq!(net_of(NetworkPolicy::Deny, false), netp(true, true, &[]));
    }

    #[test]
    fn all_leaves_connect_unhandled_but_still_handles_bind_without_local_bind() {
        assert_eq!(net_of(NetworkPolicy::All, false), netp(false, true, &[]));
        assert_eq!(net_of(NetworkPolicy::All, true), netp(false, false, &[]));
    }

    #[test]
    fn hosts_allows_80_and_443_only() {
        let hosts = NetworkPolicy::Hosts(vec!["example.com".into()]);
        assert_eq!(net_of(hosts, false), netp(true, true, &[80, 443]));
    }

    #[test]
    fn local_bind_leaves_bind_unhandled() {
        assert_eq!(net_of(NetworkPolicy::Deny, true), netp(true, false, &[]));
    }
}
