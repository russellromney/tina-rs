//! Public-corpus structural guard.
//!
//! Parses the public corpus with `syn` and rejects Tina-facing application
//! code containing:
//!
//! - `envelope-construction`: manual `ServiceMessage::Event` /
//!   `ServiceMessage::Request` construction, including through `use`/`type`
//!   aliases;
//! - `envelope-alias`: public `use` re-exports or `pub type` aliases of
//!   `ServiceMessage`;
//! - `raw-runtime-host`: production-shaped raw runtime construction
//!   (`ThreadedRuntime`, `ThreadedMultiShardRuntime`, `MultiShardRuntime`,
//!   `BridgeHost::new`, `tina_runtime::Runtime`) outside the reviewed
//!   allowlist;
//! - `manual-drain`: `shutdown_handle` / `request_and_wait_report` /
//!   `shutdown_report` / `build_keepalive_pool` / `shutdown_keepalive_pool`
//!   outside the allowlist, where a guaranteed terminal runner exists;
//! - `terminal-wildcard`: `CallOutcome` matches that collapse distinct
//!   terminals into an unnamed or ignored wildcard arm;
//! - `intent-identifier`: intent-artifact names baked into identifiers.
//!
//! Scans `examples/**/src` (minus `tokio_impl.rs`, which is the Tokio
//! control) and, for the identifier rule, public crate sources under
//! `tina*/src`. `#[cfg(test)]` items are skipped — test modules may
//! deliberately exercise low-level edge cases. `.git`, `.intent`, `target`,
//! vendored code, and lockfiles are never traversed. Allowlist validation,
//! missing roots, parse failures, stale paths, and traversal failures fail
//! closed. Pass/fail/evasion fixtures (including a directory whose path
//! contains spaces) are generated in a temp dir and driven directly below.
//!
//! Accepted limits, deliberately documented rather than hidden:
//! - macro *tokens* are opaque to `syn`; direct `ServiceMessage::Event(` /
//!   `ServiceMessage::Request(` forms inside macros are backstopped
//!   textually by `scripts/examples_service_envelope_guard.sh`.
//! - UFCS forms (`<Type>::method`) and function-pointer indirection are
//!   not resolved.
//! - `use`/`type` aliases are over-approximated to file scope (fail
//!   closed) rather than tracked per lexical scope.
//! - framework-crate `tina*/examples/` files are outside the public
//!   corpus manifest; `hello_world.rs` is instead pinned byte-for-byte
//!   to its guide quote by `tests/readme_hello_world.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use quote::ToTokens;
use serde::Deserialize;
use syn::spanned::Spanned;
use syn::visit::Visit;

/// Rules enforced by this structural (syn) guard.
const STRUCTURAL_RULES: &[&str] = &[
    "envelope-construction",
    "envelope-alias",
    "raw-runtime-host",
    "manual-drain",
    "terminal-wildcard",
    "intent-identifier",
];

/// All guard rules (structural above plus the lexical guard script's
/// `shared-state` / `poll-loop` / `obsolete-vocabulary` / `intent-phrase`
/// enforced by `scripts/public_corpus_lexical_guard.sh`) for allowlist
/// validation.
const RULES: &[&str] = &[
    "envelope-construction",
    "envelope-alias",
    "raw-runtime-host",
    "manual-drain",
    "terminal-wildcard",
    "intent-identifier",
    "shared-state",
    "poll-loop",
    "obsolete-vocabulary",
    "intent-phrase",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tina-runtime has a parent")
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowlist {
    schema: u32,
    #[serde(default, rename = "entry")]
    entries: Vec<AllowlistEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowlistEntry {
    path: String,
    rule: String,
    reason: String,
    focused_test: String,
    reviewer: String,
    reviewed_sha: String,
}

#[derive(Debug)]
struct ValidatedAllowlist {
    /// (path, rule) -> reason, for exemption lookup.
    exemptions: BTreeMap<(String, String), String>,
    entries: Vec<AllowlistEntry>,
}

fn load_allowlist(root: &Path) -> Result<ValidatedAllowlist, String> {
    let path = root.join("examples/public-corpus-allowlist.toml");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("allowlist unreadable at {}: {e}", path.display()))?;
    let parsed: Allowlist =
        toml::from_str(&text).map_err(|e| format!("allowlist parse failure: {e}"))?;
    if parsed.schema != 1 {
        return Err(format!("allowlist schema {} != 1", parsed.schema));
    }
    let mut exemptions = BTreeMap::new();
    for entry in &parsed.entries {
        if !RULES.contains(&entry.rule.as_str()) {
            return Err(format!(
                "allowlist entry {} names unknown rule {:?}",
                entry.path, entry.rule
            ));
        }
        if !root.join(&entry.path).is_file() {
            return Err(format!("allowlist entry {} names a stale path", entry.path));
        }
        for (field, value) in [
            ("reason", &entry.reason),
            ("focused_test", &entry.focused_test),
            ("reviewer", &entry.reviewer),
            ("reviewed_sha", &entry.reviewed_sha),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "allowlist entry {} has an empty {field}",
                    entry.path
                ));
            }
        }
        exemptions.insert(
            (entry.path.clone(), entry.rule.clone()),
            entry.reason.clone(),
        );
    }
    Ok(ValidatedAllowlist {
        exemptions,
        entries: parsed.entries,
    })
}

// ---------------------------------------------------------------------------
// Violations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    path: String,
    line: usize,
    rule: String,
    detail: String,
}

// ---------------------------------------------------------------------------
// File collection
// ---------------------------------------------------------------------------

fn collect_example_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let examples = root.join("examples");
    if !examples.is_dir() {
        return Err(format!("missing scan root {}", examples.display()));
    }
    let mut out = Vec::new();
    collect_rs(&examples, &mut out)?;
    out.retain(|p| p.file_name().is_some_and(|n| n != "tokio_impl.rs"));
    out.retain(|p| p.components().any(|c| c.as_os_str() == "src"));
    out.sort();
    Ok(out)
}

fn collect_crate_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let entries =
        fs::read_dir(root).map_err(|e| format!("cannot traverse {}: {e}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("traversal failure: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("tina") && entry.path().join("src").is_dir() {
            collect_rs(&entry.path().join("src"), &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot traverse {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("traversal failure: {e}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == ".git" || name == ".intent" {
                continue;
            }
            collect_rs(&path, out)?;
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-file analysis
// ---------------------------------------------------------------------------

/// Aliases local to one file: alias ident -> canonical terminal segment.
/// `use tina::ServiceMessage as Envelope;` maps Envelope -> ServiceMessage;
/// `type Runtime = tina_runtime::ThreadedRuntime<...>;` maps Runtime ->
/// ThreadedRuntime.
#[derive(Default)]
struct AliasMap {
    aliases: BTreeMap<String, String>,
    call_outcome_glob: bool,
}

const TRACKED_TYPES: &[&str] = &[
    // Longest names first: a type alias text that mentions both
    // `ThreadedRuntime` and `Runtime` must resolve to the specific one.
    "ThreadedMultiShardRuntime",
    "ThreadedRuntime",
    "MultiShardRuntime",
    "ServiceMessage",
    "BridgeHost",
    "Runtime",
];

fn build_alias_map(file: &syn::File) -> AliasMap {
    struct Collector {
        map: AliasMap,
    }
    impl Visit<'_> for Collector {
        fn visit_item_use(&mut self, item: &syn::ItemUse) {
            collect_use_aliases(&item.tree, &mut Vec::new(), &mut self.map);
            syn::visit::visit_item_use(self, item);
        }

        fn visit_item_type(&mut self, item: &syn::ItemType) {
            let text = item.ty.to_token_stream().to_string();
            for tracked in TRACKED_TYPES {
                if text.contains(tracked) {
                    self.map
                        .aliases
                        .insert(item.ident.to_string(), tracked.to_string());
                    break;
                }
            }
            syn::visit::visit_item_type(self, item);
        }
    }
    // Walk the whole file, not just top-level items: aliases declared in fn
    // bodies or inner modules are over-approximated to file scope (fail
    // closed) rather than missed.
    let mut collector = Collector {
        map: AliasMap::default(),
    };
    collector.visit_file(file);
    collector.map
}

fn collect_use_aliases(tree: &syn::UseTree, prefix: &mut Vec<String>, map: &mut AliasMap) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_aliases(&path.tree, prefix, map);
            prefix.pop();
        }
        syn::UseTree::Group(group) => {
            for tree in &group.items {
                collect_use_aliases(tree, &mut prefix.clone(), map);
            }
        }
        syn::UseTree::Name(name) => {
            let ident = name.ident.to_string();
            if TRACKED_TYPES.contains(&ident.as_str()) || ident == "CallOutcome" {
                map.aliases.insert(ident.clone(), ident);
            } else if (ident == "Event" || ident == "Request")
                && prefix.last().is_some_and(|last| last == "ServiceMessage")
            {
                // `use tina::ServiceMessage::Event;` then bare `Event(..)`.
                map.aliases
                    .insert(ident.clone(), format!("ServiceMessage::{ident}"));
            }
        }
        syn::UseTree::Rename(rename) => {
            let from = rename.ident.to_string();
            if TRACKED_TYPES.contains(&from.as_str()) || from == "CallOutcome" {
                map.aliases.insert(rename.rename.to_string(), from);
            }
        }
        syn::UseTree::Glob(_) => {
            if prefix.last().is_some_and(|last| last == "CallOutcome") {
                map.call_outcome_glob = true;
            }
        }
    }
}

struct GuardVisitor<'a> {
    aliases: &'a AliasMap,
    path: String,
    violations: Vec<Violation>,
    cfg_test_depth: usize,
}

impl GuardVisitor<'_> {
    fn push(&mut self, span: proc_macro2::Span, rule: &str, detail: String) {
        if self.cfg_test_depth > 0 {
            return;
        }
        self.violations.push(Violation {
            path: self.path.clone(),
            line: span.start().line,
            rule: rule.to_string(),
            detail,
        });
    }

    /// Resolve a path's leading segment through the alias map, returning the
    /// canonical terminal segment of the whole path when tracked.
    fn resolve_path(&self, path: &syn::Path) -> Option<(String, String)> {
        let mut segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if segments.is_empty() {
            return None;
        }
        if let Some(canonical) = self.aliases.aliases.get(&segments[0]) {
            if canonical.contains("::") && segments.len() == 1 {
                segments = canonical.split("::").map(str::to_string).collect();
            } else {
                segments[0] = canonical.clone();
            }
        }
        let terminal = segments.last()?.clone();
        let joined = segments.join("::");
        Some((terminal, joined))
    }

    fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
        attrs.iter().any(|attr| {
            if !attr.path().is_ident("cfg") {
                return false;
            }
            attr.parse_args::<syn::Ident>()
                .is_ok_and(|ident| ident == "test")
        })
    }

    fn check_constructor(&mut self, span: proc_macro2::Span, path: &syn::Path) {
        let Some((terminal, joined)) = self.resolve_path(path) else {
            return;
        };
        match terminal.as_str() {
            "Event" | "Request" => {
                let segments: Vec<&str> = joined.split("::").collect();
                if segments.len() >= 2 && segments[segments.len() - 2] == "ServiceMessage" {
                    self.push(
                        span,
                        "envelope-construction",
                        format!("manual ServiceMessage::{terminal} construction via `{joined}`"),
                    );
                }
            }
            _ => {
                let segments: Vec<&str> = joined.split("::").collect();
                if segments.len() < 2 {
                    return;
                }
                let type_seg = segments[segments.len() - 2];
                let method = terminal.as_str();
                let raw_host = matches!(
                    type_seg,
                    "ThreadedRuntime" | "ThreadedMultiShardRuntime" | "MultiShardRuntime"
                ) && (method == "new"
                    || method == "try_new"
                    || method.starts_with("with_config")
                    || method.starts_with("try_with_config"))
                    || (type_seg == "BridgeHost" && method == "new")
                    || (type_seg == "Runtime" && (method == "new" || method == "with_config"));
                if raw_host {
                    self.push(
                        span,
                        "raw-runtime-host",
                        format!("production-shaped raw runtime construction `{joined}`"),
                    );
                }
                let keepalive_free_fn =
                    terminal == "build_keepalive_pool" || terminal == "shutdown_keepalive_pool";
                if keepalive_free_fn {
                    self.push(
                        span,
                        "manual-drain",
                        format!("manual keepalive lifecycle `{joined}`"),
                    );
                }
            }
        }
    }

    fn check_match(&mut self, expr_match: &syn::ExprMatch) {
        let arm_text = |arm: &syn::Arm| arm.pat.to_token_stream().to_string();
        let mentions_call_outcome = expr_match.arms.iter().any(|arm| {
            let text = arm_text(arm);
            if text.contains("CallOutcome") {
                return true;
            }
            // `use tina_runtime::CallOutcome as CO;` then `CO::Replied(_)`.
            if self.aliases.aliases.iter().any(|(alias, canonical)| {
                canonical == "CallOutcome" && text.contains(alias.as_str())
            }) {
                return true;
            }
            // `use tina_runtime::CallOutcome::*;` then `Replied(_)`, `_ => ..`.
            self.aliases.call_outcome_glob
                && text.split([':', '(']).next().is_some_and(|head| {
                    matches!(
                        head.trim(),
                        "Replied" | "Full" | "Closed" | "Timeout" | "Rejected"
                    )
                })
        });
        if !mentions_call_outcome {
            return;
        }
        for arm in &expr_match.arms {
            match &arm.pat {
                syn::Pat::Wild(wild) => {
                    self.push(
                        wild.underscore_token.span,
                        "terminal-wildcard",
                        "CallOutcome terminals collapsed into an unnamed `_` arm".to_string(),
                    );
                }
                syn::Pat::Ident(ident) => {
                    let name = ident.ident.to_string();
                    let body = arm.body.to_token_stream().to_string();
                    if !body.contains(&name) {
                        self.push(
                            ident.ident.span(),
                            "terminal-wildcard",
                            format!(
                                "CallOutcome terminals collapsed into ignored binding `{name}`"
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn check_ident(&mut self, span: proc_macro2::Span, ident: &syn::Ident) {
        let normalized: String = ident
            .to_string()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        for forbidden in ["publicexamplecertification", "executionreview"] {
            if normalized.contains(forbidden) {
                self.push(
                    span,
                    "intent-identifier",
                    format!("identifier `{ident}` leaks an intent-artifact name"),
                );
            }
        }
    }
}

impl Visit<'_> for GuardVisitor<'_> {
    fn visit_variant(&mut self, variant: &syn::Variant) {
        self.check_ident(variant.ident.span(), &variant.ident);
        syn::visit::visit_variant(self, variant);
    }

    fn visit_field(&mut self, field: &syn::Field) {
        if let Some(ident) = &field.ident {
            self.check_ident(ident.span(), ident);
        }
        syn::visit::visit_field(self, field);
    }

    fn visit_local(&mut self, local: &syn::Local) {
        if let syn::Pat::Ident(pat) = &local.pat {
            self.check_ident(pat.ident.span(), &pat.ident);
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_item_mod(&mut self, item: &syn::ItemMod) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_item_fn(&mut self, item: &syn::ItemFn) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.sig.ident.span(), &item.sig.ident);
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_item_struct(&mut self, item: &syn::ItemStruct) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &syn::ItemEnum) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_type(&mut self, item: &syn::ItemType) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        let text = item.ty.to_token_stream().to_string();
        let names_service_message =
            text.contains("ServiceMessage")
                || self.aliases.aliases.iter().any(|(alias, canonical)| {
                    canonical == "ServiceMessage" && text.contains(alias)
                });
        if names_service_message && matches!(item.vis, syn::Visibility::Public(_)) {
            self.push(
                item.ident.span(),
                "envelope-alias",
                format!("public type alias `{}` names ServiceMessage", item.ident),
            );
        }
        syn::visit::visit_item_type(self, item);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        let text = item.to_token_stream().to_string();
        if text.contains("ServiceMessage") && matches!(item.vis, syn::Visibility::Public(_)) {
            self.push(
                item.use_token.span,
                "envelope-alias",
                "public `use` re-export names ServiceMessage".to_string(),
            );
        }
        syn::visit::visit_item_use(self, item);
    }

    fn visit_item_trait(&mut self, item: &syn::ItemTrait) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_trait(self, item);
    }

    fn visit_item_const(&mut self, item: &syn::ItemConst) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &syn::ItemStatic) {
        if Self::is_cfg_test(&item.attrs) {
            return;
        }
        self.check_ident(item.ident.span(), &item.ident);
        syn::visit::visit_item_static(self, item);
    }

    fn visit_expr_call(&mut self, expr: &syn::ExprCall) {
        if let syn::Expr::Path(path) = expr.func.as_ref() {
            self.check_constructor(path.path.span(), &path.path);
        }
        syn::visit::visit_expr_call(self, expr);
    }

    fn visit_expr_struct(&mut self, expr: &syn::ExprStruct) {
        self.check_constructor(expr.path.span(), &expr.path);
        syn::visit::visit_expr_struct(self, expr);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        self.check_constructor(mac.path.span(), &mac.path);
        syn::visit::visit_macro(self, mac);
    }

    fn visit_expr_method_call(&mut self, expr: &syn::ExprMethodCall) {
        let method = expr.method.to_string();
        if matches!(
            method.as_str(),
            "shutdown_report" | "request_and_wait_report" | "shutdown_handle"
        ) {
            self.push(
                expr.method.span(),
                "manual-drain",
                format!("manual runtime lifecycle `.{method}()`"),
            );
        }
        syn::visit::visit_expr_method_call(self, expr);
    }

    fn visit_expr_match(&mut self, expr: &syn::ExprMatch) {
        self.check_match(expr);
        syn::visit::visit_expr_match(self, expr);
    }
}

fn scan_file(path: &Path, rel: &Path) -> Result<Vec<Violation>, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let parsed =
        syn::parse_file(&text).map_err(|e| format!("parse failure in {}: {e}", path.display()))?;
    let aliases = build_alias_map(&parsed);
    let mut visitor = GuardVisitor {
        aliases: &aliases,
        path: rel.to_string_lossy().replace('\\', "/"),
        violations: Vec::new(),
        cfg_test_depth: 0,
    };
    visitor.visit_file(&parsed);
    Ok(visitor.violations)
}

fn scan_corpus(root: &Path) -> Result<Vec<Violation>, String> {
    let mut violations = Vec::new();
    for file in collect_example_sources(root)? {
        let rel = file.strip_prefix(root).expect("under root").to_path_buf();
        violations.extend(scan_file(&file, &rel)?);
    }
    for file in collect_crate_sources(root)? {
        let rel = file.strip_prefix(root).expect("under root").to_path_buf();
        // Public crate sources legitimately construct envelopes and own raw
        // runtimes internally; only the intent-identifier rule applies.
        violations.extend(
            scan_file(&file, &rel)?
                .into_iter()
                .filter(|v| v.rule == "intent-identifier"),
        );
    }
    violations.sort();
    Ok(violations)
}

// ---------------------------------------------------------------------------
// Production scan
// ---------------------------------------------------------------------------

#[test]
fn public_corpus_structural_scan() {
    let root = repo_root();
    let allowlist = load_allowlist(&root).expect("allowlist must validate");
    let violations = scan_corpus(&root).expect("corpus scan must complete");

    let mut live = Vec::new();
    let mut matched: BTreeSet<(String, String)> = BTreeSet::new();
    for violation in &violations {
        let key = (violation.path.clone(), violation.rule.clone());
        if allowlist.exemptions.contains_key(&key) {
            matched.insert(key);
        } else {
            live.push(violation);
        }
    }

    let mut failures = Vec::new();
    if !live.is_empty() {
        failures.push(format!(
            "public-corpus guard: {} unexplained violation(s):",
            live.len()
        ));
        for violation in &live {
            failures.push(format!(
                "  {}:{} [{}] {}",
                violation.path, violation.line, violation.rule, violation.detail
            ));
        }
    }
    for entry in &allowlist.entries {
        // Only structural rules are validated for staleness here; the lexical
        // guard validates its own rules against the same file.
        if !STRUCTURAL_RULES.contains(&entry.rule.as_str()) {
            continue;
        }
        let key = (entry.path.clone(), entry.rule.clone());
        if !matched.contains(&key) {
            failures.push(format!(
                "stale allowlist entry: {} [{}] no longer matches a live guard hit",
                entry.path, entry.rule
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!(
        "public-corpus structural guard: ok ({} files, {} allowlisted forms)",
        collect_example_sources(&root).map(|f| f.len()).unwrap_or(0)
            + collect_crate_sources(&root).map(|f| f.len()).unwrap_or(0),
        matched.len(),
    );
}

// ---------------------------------------------------------------------------
// Fixtures: pass / fail / evasion, driven directly in a temp dir.
// ---------------------------------------------------------------------------

fn fixture_root(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "public corpus guard {tag} {} {}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("examples/specimen_demo/src")).expect("create fixture src");
    fs::create_dir_all(dir.join("tina-fake/src")).expect("create fixture crate src");
    dir
}

fn write_fixture(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write fixture");
}

fn scan_fixture(root: &Path) -> Vec<Violation> {
    scan_corpus(root).expect("fixture scan")
}

#[test]
fn fixture_clean_facade_passes() {
    let root = fixture_root("clean");
    write_fixture(
        &root,
        "examples/specimen_demo/src/tina_impl.rs",
        r#"
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};
use tina::prelude::*;

pub fn run() -> anyhow::Result<()> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(std::time::Duration::from_secs(5), workload)?)
}

fn workload(_app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>) -> anyhow::Result<()> {
    Ok(())
}

fn classify(outcome: tina_runtime::CallOutcome<u64>) -> &'static str {
    match outcome {
        tina_runtime::CallOutcome::Replied(_) => "replied",
        other => panic!("terminal must stay distinct: {other:?}"),
    }
}
"#,
    );
    assert_eq!(scan_fixture(&root), Vec::new());
}

#[test]
fn fixture_direct_and_aliased_violations_fail() {
    let root = fixture_root("fail");
    write_fixture(
        &root,
        "examples/specimen_demo/src/tina_impl.rs",
        r#"
use tina::ServiceMessage as Envelope;
use tina_runtime::{ThreadedRuntime, ThreadedMultiShardRuntime as Multi};
use tina_runtime::DefaultThreadedMailboxFactory;
use tina::prelude::*;

type Rt = tina_runtime::ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;

pub type PublicEnvelope<E, R> = Envelope<E, R>;

pub fn bad() {
    let _ = Envelope::Event(42u32);
    let _ = tina::ServiceMessage::Event(7u32);
    let _ = tina::ServiceMessage::Request { payload: 9u32 };
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
    let _ = Multi::try_new([SingleShard], DefaultThreadedMailboxFactory).unwrap();
    let _ = Rt::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
    let shutdown = runtime.shutdown_handle();
    let _ = shutdown.request_and_wait_report(std::time::Duration::from_secs(1));
    let _ = runtime.shutdown_report();
}

pub fn collapse(outcome: tina_runtime::CallOutcome<u64>) -> &'static str {
    match outcome {
        tina_runtime::CallOutcome::Replied(_) => "ok",
        _ => "collapsed",
    }
}

pub fn collapse_named(outcome: tina_runtime::CallOutcome<u64>) -> &'static str {
    match outcome {
        tina_runtime::CallOutcome::Replied(_) => "ok",
        other => "ignored",
    }
}
"#,
    );
    let violations = scan_fixture(&root);
    let rules: Vec<(&str, usize)> = violations
        .iter()
        .map(|v| (v.rule.as_str(), v.line))
        .collect();
    let count = |rule: &str| rules.iter().filter(|(r, _)| *r == rule).count();
    assert_eq!(count("envelope-construction"), 3, "{violations:?}");
    assert_eq!(count("envelope-alias"), 1, "{violations:?}");
    assert_eq!(count("raw-runtime-host"), 3, "{violations:?}");
    assert_eq!(count("manual-drain"), 3, "{violations:?}");
    assert_eq!(count("terminal-wildcard"), 2, "{violations:?}");
}

#[test]
fn fixture_cfg_test_items_are_skipped() {
    let root = fixture_root("cfgtest");
    write_fixture(
        &root,
        "examples/specimen_demo/src/lib.rs",
        r#"
pub fn ok() {}

#[cfg(test)]
mod tests {
    use tina_runtime::{ThreadedRuntime, DefaultThreadedMailboxFactory};
    use tina::prelude::*;

    #[test]
    fn low_level_edge() {
        let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
        let _ = runtime.shutdown_handle();
    }
}
"#,
    );
    assert_eq!(scan_fixture(&root), Vec::new());
}

#[test]
fn fixture_low_level_evasions_fail() {
    let root = fixture_root("evasion");
    write_fixture(
        &root,
        "examples/specimen_demo/src/main.rs",
        r#"
use tina_runtime::{DefaultMailboxFactory, MultiShardRuntime, Runtime};
use tina_runtime::BridgeHost;

fn main() {
    let _ = MultiShardRuntime::new([1u32], DefaultMailboxFactory);
    let _ = Runtime::new(1u32, DefaultMailboxFactory);
    let _ = BridgeHost::new(1u32, DefaultMailboxFactory, 4);
    let _ = tina_http::build_keepalive_pool();
    let _ = tina_http::shutdown_keepalive_pool();
}
"#,
    );
    write_fixture(
        &root,
        "tina-fake/src/lib.rs",
        r#"
pub fn public_example_certification_runner() {}

pub struct ExecutionReviewFixture;

pub enum Reviewed {
    ExecutionReviewOne,
}

pub struct Counts {
    pub public_example_certification_count: u32,
}

pub fn bind() {
    let execution_review = 1u32;
}
"#,
    );
    let violations = scan_fixture(&root);
    let count = |rule: &str| violations.iter().filter(|v| v.rule == rule).count();
    assert_eq!(count("raw-runtime-host"), 3, "{violations:?}");
    assert_eq!(count("manual-drain"), 2, "{violations:?}");
    assert_eq!(count("intent-identifier"), 5, "{violations:?}");
}

#[test]
fn fixture_import_and_scope_evasions_fail() {
    let root = fixture_root("import evasions");
    write_fixture(
        &root,
        "examples/specimen_demo/src/tina_impl.rs",
        r#"
use tina::ServiceMessage::Event;
use tina_runtime::CallOutcome::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};
use tina::prelude::*;

pub fn variant_import() {
    let _ = Event(1u32);
}

pub fn glob_collapse(outcome: tina_runtime::CallOutcome<u64>) -> &'static str {
    match outcome {
        Replied(_) => "ok",
        _ => "collapsed",
    }
}

pub fn nested_scope() {
    type Rt = tina_runtime::ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;
    let _ = Rt::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
}
"#,
    );
    let violations = scan_fixture(&root);
    let count = |rule: &str| violations.iter().filter(|v| v.rule == rule).count();
    assert_eq!(count("envelope-construction"), 1, "{violations:?}");
    assert_eq!(count("terminal-wildcard"), 1, "{violations:?}");
    assert_eq!(count("raw-runtime-host"), 1, "{violations:?}");
}

#[test]
fn fixture_allowlist_exempts_and_staleness_fails() {
    let root = fixture_root("allowlist");
    write_fixture(
        &root,
        "examples/specimen_demo/src/main.rs",
        r#"
use tina_runtime::{ThreadedRuntime, DefaultThreadedMailboxFactory};
use tina::prelude::*;

fn main() {
    let _ = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
}
"#,
    );
    // Well-formed allowlist exempts the hit.
    write_fixture(
        &root,
        "examples/public-corpus-allowlist.toml",
        r#"
schema = 1

[[entry]]
path = "examples/specimen_demo/src/main.rs"
rule = "raw-runtime-host"
reason = "fixture"
focused_test = "fixture-test"
reviewer = "fixture"
reviewed_sha = "00000000"
"#,
    );
    let allowlist = load_allowlist(&root).expect("valid allowlist");
    assert!(allowlist.exemptions.contains_key(&(
        "examples/specimen_demo/src/main.rs".to_string(),
        "raw-runtime-host".to_string()
    )));

    // Unknown rule name fails closed.
    write_fixture(
        &root,
        "examples/public-corpus-allowlist-bad-rule.toml",
        r#"
schema = 1

[[entry]]
path = "examples/specimen_demo/src/main.rs"
rule = "not-a-rule"
reason = "fixture"
focused_test = "fixture-test"
reviewer = "fixture"
reviewed_sha = "00000000"
"#,
    );
    write_fixture(
        &root,
        "examples/public-corpus-allowlist.toml",
        r#"
schema = 1

[[entry]]
path = "examples/specimen_demo/src/main.rs"
rule = "not-a-rule"
reason = "fixture"
focused_test = "fixture-test"
reviewer = "fixture"
reviewed_sha = "00000000"
"#,
    );
    let err = load_allowlist(&root).expect_err("unknown rule must fail");
    assert!(err.contains("unknown rule"), "{err}");

    // Stale entry path fails closed through the real loader.
    write_fixture(
        &root,
        "examples/public-corpus-allowlist.toml",
        r#"
schema = 1

[[entry]]
path = "examples/specimen_demo/src/deleted.rs"
rule = "raw-runtime-host"
reason = "fixture"
focused_test = "fixture-test"
reviewer = "fixture"
reviewed_sha = "00000000"
"#,
    );
    let err = load_allowlist(&root).expect_err("stale path must fail");
    assert!(err.contains("stale path"), "{err}");

    // Unknown field fails closed at parse time.
    let bad = r#"
schema = 1

[[entry]]
path = "x"
rule = "raw-runtime-host"
reason = "r"
focused_test = "t"
reviewer = "r"
reviewed_sha = "s"
surprise = "field"
"#;
    assert!(toml::from_str::<Allowlist>(bad).is_err());
}

#[test]
fn fixture_parse_failure_fails_closed() {
    let root = fixture_root("parse");
    write_fixture(
        &root,
        "examples/specimen_demo/src/broken.rs",
        "fn broken( { not rust",
    );
    assert!(scan_corpus(&root).is_err());
}

#[test]
fn fixture_missing_scan_root_fails_closed() {
    let root = std::env::temp_dir().join(format!(
        "public corpus guard missing-root {}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("mkdir");
    assert!(collect_example_sources(&root).is_err());
}
