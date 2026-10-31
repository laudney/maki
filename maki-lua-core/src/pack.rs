//! Discovery of external packages installed under the site directory.
//!
//! This is the manual half of the package model: directories a user cloned
//! themselves, laid out the way Neovim lays packages out. Packages that maki
//! installs are resolved from recorded state instead, and never appear here.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_pack::approvals::{ApprovalKey, Approvals, Entry};
use maki_pack::lockfile::{Lockfile, Owner};
use tokio::runtime::Handle;

use crate::bundled::Bundled;
use crate::error::PluginError;
use crate::plugin_permissions::{
    Component, PackageRequests, Permission, Requested, load_requested_permissions,
    requested_permissions_from_text,
};

const ACTIVE_DELETE_REFUSAL: &str = "package is active and was not removed";
const OWNER_CONFLICT_FAILURE: &str = "package name conflicts with another owner";
const DECLARED_DELETE_REFUSAL: &str = "still declared; remove it from maki.pack.add first";
const UPDATE_USAGE: &str = "/packupdate: name at most one package";
const DELETE_USAGE: &str = "/packdel: name a package, or pass ++all";
const PLUGIN_MANIFEST: &str = "plugin.toml";
/// Sets a host apart from the permission names it is listed beside when a
/// package still needs approving.
const NET_HOST_LABEL: &str = "net host";
/// How a reach of every host is listed beside the named ones.
const ANY_HOST: &str = "*";
/// Sets a taken-over catalog name apart from the permission names it is
/// listed beside when a package still needs approving, in the install prompt
/// and in the update diff alike.
const PROVIDER_TAKEOVER_LABEL: &str = "takes over provider";
const APPROVALS_FILE: &str = "pack-approvals.json";
const UI_APPROVALS_FILE: &str = "pack-ui-approvals.json";
/// What the UI prompt lists for a UI component that asks for nothing: it is
/// still new code, so running it at all is the question.
const NO_PERMISSIONS: &str = "no permissions";
const UI_APPROVAL_REQUIRED: &str = "UI approval is required";
const UI_APPROVAL_STORE_UNREADABLE: &str =
    "the package UI approval store is unreadable, so its UI was not approved";
/// Starts the update review's line for the UI component, under the agent's.
const UI_REVIEW_LABEL: &str = "ui permissions";
/// The update review's UI line for a component never approved before, so an
/// empty request does not read as "unchanged".
const UI_REVIEW_NEW: &str = "new";
const REVIEW_PROMPT_HEADER: &str = "Apply these package changes?";
const REVIEW_DELETE_LINE: &str = "remove";
/// A host that loads only the UI deletes only the UI of a package the agent
/// also owns.
const REVIEW_RELEASE_LINE: &str = "remove its UI; the agent keeps the checkout";
/// Only the UI host loads one component and takes package commands, so the
/// other owner is the agent.
const SHARED_UPDATE_REFUSAL: &str = "the agent also owns this package, so update it where the agent loads, which reviews both components";
const UNOWNED_DELETE_REFUSAL: &str = "the agent's package, not available for a remote agent";
const CONFLICTING_DECLARATIONS: &str = "agent and UI package declarations differ";
const FOREIGN_SOURCE_CONFLICT: &str = "declared from another source than the package of this name another component owns, so it was not installed or loaded here";
const UNAVAILABLE_OLD_REVISION: &str = "\n  previous revision unavailable";
const REVOKE_APPROVAL_FAILURE: &str = "removed, but its approval could not be revoked";
const REVOKE_UI_APPROVAL_FAILURE: &str = "removed, but its UI approval could not be revoked";
const SAVE_APPROVAL_FAILURE: &str = "updated, but its permission approval could not be saved";
const SAVE_UI_APPROVAL_FAILURE: &str = "updated, but its UI approval could not be saved";

impl From<Component> for Owner {
    fn from(component: Component) -> Self {
        match component {
            Component::Agent => Self::Agent,
            Component::Ui => Self::Ui,
        }
    }
}

fn owners_of(components: impl IntoIterator<Item = Component>) -> Vec<Owner> {
    components.into_iter().map(Owner::from).collect()
}

/// Whether a host that loads {components} owns every part of {name} that
/// anyone does, so it may change the checkout itself. A name the lockfile
/// does not record has no owner to protect, and the change fails on its
/// own terms.
fn owns_all(lock: &Lockfile, name: &str, components: &[Component]) -> bool {
    let owners = owners_of(components.iter().copied());
    lock.get(name)
        .is_none_or(|entry| entry.owners.iter().all(|owner| owners.contains(owner)))
}

/// Whether a host that loads {components} owns any part of {name}, as
/// [`owns_all`] counts it.
fn owns_any(lock: &Lockfile, name: &str, components: &[Component]) -> bool {
    let owners = owners_of(components.iter().copied());
    lock.get(name)
        .is_none_or(|entry| entry.owners.iter().any(|owner| owners.contains(owner)))
}

pub fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                ' '
            }
        })
        .collect()
}

/// The group name reserved for packages maki installs itself. Manual discovery
/// skips it, so one package can never be found twice, once from disk and once
/// from recorded state, with the two disagreeing about its revision.
///
/// Re-exported rather than repeated: `maki-pack` decides where it puts a
/// checkout, and a second copy here that drifted would stop discovery skipping
/// the directory it writes.
pub use maki_pack::paths::MANAGED_GROUP;

/// `<data>/site`, the root Neovim would call a package path.
pub fn site_dir() -> Result<PathBuf, std::io::Error> {
    maki_storage::paths::data_dir().map(|d| d.join("site"))
}

/// How a package reached the disk, which is what decides whose word grants its
/// permissions.
///
/// This is a distinction the code needs and did not have. A manifest is written
/// by whoever wrote the package. For a package the user placed by hand that is
/// effectively the user, so the manifest is their own statement of intent. For
/// a package maki fetched it is a stranger, and letting the manifest grant
/// itself permissions would make the request self-certifying: a later revision
/// could add `run = true` and start subprocesses without anyone agreeing to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The user put the files under `pack/<group>/`. Trusted like `init.lua`.
    Manual,
    /// Maki cloned it from this source. The source is part of the approval key,
    /// so re-pointing a name at another repository does not inherit its grants.
    Fetched { src: String },
}

#[derive(Debug, Clone)]
pub struct DiscoveredPackage {
    pub name: String,
    /// Canonical package root. Resolved once, here, so the manifest, the
    /// entrypoints, and every later `require` agree on one directory.
    pub dir: PathBuf,
    /// `start/` packages load at startup; `opt/` packages wait to be activated.
    pub eager: bool,
    /// What each component of the package's manifest asks for. Never a
    /// grant on its own for a fetched package; see `Origin`.
    pub requested: PackageRequests,
    pub origin: Origin,
    pub revision_guard: Option<Arc<maki_pack::lock::Lock>>,
}

impl DiscoveredPackage {
    /// The key both components' approvals are filed under, `None` for a
    /// hand-installed package, whose manifest grants itself.
    fn approval_key(&self) -> Option<ApprovalKey> {
        match &self.origin {
            Origin::Manual => None,
            Origin::Fetched { src } => Some(ApprovalKey::new(self.name.clone(), src)),
        }
    }
}

/// `pack-lock.json`, beside the user's configuration so it can be committed.
///
/// `config_search_dirs` returns candidates in the order `init.lua` is searched
/// for, so the first that exists is the one whose `maki.pack.add` declared
/// these packages, and a custom config directory keeps its lockfile rather than
/// leaving it behind in XDG. With none of them created yet there is nothing to
/// sit beside, and the last candidate is the one `append_permission_rule`
/// already treats as writable.
pub fn lockfile_path() -> Option<PathBuf> {
    let dirs = maki_storage::paths::config_search_dirs();
    dirs.iter()
        .find(|dir| dir.is_dir())
        .cloned()
        .or_else(|| dirs.into_iter().next_back())
        .map(|dir| dir.join("pack-lock.json"))
}

/// The file in the site holding {component}'s approvals.
///
/// The UI's has a file of its own rather than a field in the agent's, so every
/// approval written before components existed is an agent approval as it
/// stands, and an older build, which never opens the UI file, can neither drop
/// nor misread a UI approval.
fn approvals_file(component: Component) -> &'static str {
    match component {
        Component::Agent => APPROVALS_FILE,
        Component::Ui => UI_APPROVALS_FILE,
    }
}

/// {component}'s approval store in {site}.
pub fn approvals_path_in(site: &Path, component: Component) -> PathBuf {
    site.join(approvals_file(component))
}

/// {component}'s approval store, in the state directory beside the checkouts.
///
/// Deliberately not beside the lockfile. A lockfile is meant to be committed,
/// so a package set reproduces on another machine. An approval is the opposite
/// kind of fact: one person's decision to trust one repository on one machine,
/// which must not travel with a repository into someone else's checkout.
pub fn approvals_path(component: Component) -> Option<PathBuf> {
    site_dir()
        .ok()
        .map(|dir| approvals_path_in(&dir, component))
}

fn read_approvals_file(path: &Path) -> Option<Approvals> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(Approvals::default());
        }
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                %error,
                "pack approval store could not be read; granting nothing"
            );
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(approvals) => Some(approvals),
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "pack approval store is unreadable; granting nothing"
            );
            None
        }
    }
}

/// Each component's approval store, read once for a pass of package loads.
#[derive(Debug, Default)]
pub struct ComponentApprovals {
    agent: Approvals,
    ui: Approvals,
}

impl ComponentApprovals {
    pub fn new(agent: Approvals, ui: Approvals) -> Self {
        Self { agent, ui }
    }

    /// The stores of {components} on disk; any other component's reads as
    /// approving nothing, since nothing of it loads.
    pub fn read(components: &[Component]) -> Self {
        site_dir().map_or_else(|_| Self::default(), |site| Self::read_in(&site, components))
    }

    /// As [`ComponentApprovals::read`], from the stores in {site}.
    pub fn read_in(site: &Path, components: &[Component]) -> Self {
        let read = |component| {
            if components.contains(&component) {
                read_approvals_file(&approvals_path_in(site, component)).unwrap_or_default()
            } else {
                Approvals::default()
            }
        };
        Self::new(read(Component::Agent), read(Component::Ui))
    }

    pub fn of(&self, component: Component) -> &Approvals {
        match component {
            Component::Agent => &self.agent,
            Component::Ui => &self.ui,
        }
    }
}

/// Effective permissions, and the slugs it may register, for {component} of
/// a package about to load, against {component}'s store.
///
/// The whole point of `Origin`: a fetched package's own manifest is a request,
/// and a request is not a grant. The store is passed in so the rule can be
/// tested against a store built in the test rather than against whatever the
/// person running the tests happens to have approved.
pub fn granted(
    pkg: &DiscoveredPackage,
    approvals: &Approvals,
    component: Component,
) -> (crate::plugin_permissions::PluginPermissions, Vec<String>) {
    let requested = pkg.requested.of(component);
    let Some(key) = pkg.approval_key() else {
        return (requested.clone().granted(), requested.providers().to_vec());
    };
    let approved = approvals.get(&key);
    let permissions = crate::plugin_permissions::PluginPermissions::from_approved(
        approved
            .map(Entry::permissions)
            .unwrap_or_default()
            .iter()
            .map(String::as_str),
    );
    let providers = requested.granted_providers(approved.map(Entry::providers).unwrap_or_default());
    (requested.intersect(&permissions), providers)
}

/// What {component} of {pkg} asks for that {component}'s store has not
/// approved; nothing for a hand-installed package, which approves itself.
pub fn missing_grants(
    pkg: &DiscoveredPackage,
    approvals: &Approvals,
    component: Component,
) -> Vec<String> {
    pkg.approval_key().map_or_else(Vec::new, |key| {
        missing_requests(pkg.requested.of(component), approvals.get(&key))
    })
}

/// Whether {component} of {pkg} may load, against {component}'s store.
///
/// Each component passes the gate a package passed before components
/// existed: a fetched one needs every grant it asks for approved, and a
/// hand-installed one is the user's own code. The UI entry of a fetched
/// package is new code on top of that, so it needs a UI approval record of
/// its own, even an empty one: an agent approval never vouches for it.
pub fn eligible(pkg: &DiscoveredPackage, approvals: &Approvals, component: Component) -> bool {
    let Some(key) = pkg.approval_key() else {
        return true;
    };
    let approved = approvals.get(&key);
    let recorded = component == Component::Agent || approved.is_some();
    recorded && missing_requests(pkg.requested.of(component), approved).is_empty()
}

/// Why {component} of {pkg} is not [`eligible`] against {approvals}, as
/// the install pass words it, so a host that gates it again where it loads
/// reports the same line.
pub fn ineligible_warning(
    pkg: &DiscoveredPackage,
    approvals: &Approvals,
    component: Component,
) -> String {
    approval_required(
        &pkg.name,
        component,
        &missing_grants(pkg, approvals, component),
    )
}

/// Whether {component} of {pkg} is one to put to the user: a fetched
/// package's component its manifest speaks for that is not yet eligible.
fn needs_approval(pkg: &DiscoveredPackage, approvals: &Approvals, component: Component) -> bool {
    pkg.requested.declares(component) && !eligible(pkg, approvals, component)
}

/// Whether someone is there to answer. `Tty` is the TUI: it can confirm an
/// install on the terminal before it starts, and its loop answers `maki.ui`
/// calls once it runs. `None` is `maki -p`, the sdk and ACP.
///
/// An install runs downloaded code, so it is a trust decision. With nobody to
/// ask we refuse it rather than assume consent. Lua also gets no UI sender
/// then, so a UI call fails right away instead of waiting forever for a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    Tty,
    None,
}

impl Interaction {
    pub fn confirm(self, prompt: &str) -> bool {
        if self != Self::Tty || !io::stdin().is_terminal() {
            return false;
        }
        eprint!("{} [y/N] ", sanitize_message(prompt));
        let _ = io::stderr().flush();
        let mut input = String::new();
        io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
    }
}

/// What an install pass produced, and what it could not.
///
/// Failures are returned rather than only logged: installation runs before
/// logging is set up, so a message written there reaches nobody.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub packages: Vec<DiscoveredPackage>,
    pub failures: Vec<String>,
}

/// Whether a declared package is already installed from the source it names.
///
/// Both halves matter. A recorded revision only describes the source it was
/// recorded for, so a name pointed at a different repository is neither
/// installed nor covered by the decision that trusted the old one.
fn installed_from_declared_source(
    declared: &crate::api::pack::Declared,
    lock: &maki_pack::lockfile::Lockfile,
    manager: &maki_pack::manager::Manager,
) -> bool {
    let spec = &declared.spec;
    lock.get(&spec.name)
        .is_some_and(|entry| entry.src == spec.src)
        && manager.resolve(lock, &spec.name).is_some()
}

/// Resolves one declared package from recorded state alone: no git, no lock on
/// the lockfile, no network.
///
/// `None` without a failure means the package is simply not installed from the
/// source it names. A package that is installed but unusable pushes a reason,
/// because otherwise it would vanish with nothing said about why.
///
/// Shared by the install path and the read-only fallback so the two cannot
/// drift: both must take the same revision guard and read the same manifest,
/// and a guard missed on one side would let a live revision be pruned.
fn resolve_declared(
    declared: &crate::api::pack::Declared,
    site: &Path,
    manager: &maki_pack::manager::Manager,
    lock: &maki_pack::lockfile::Lockfile,
    failures: &mut Vec<String>,
) -> Option<DiscoveredPackage> {
    let spec = &declared.spec;
    let entry = lock.get(&spec.name).filter(|entry| entry.src == spec.src)?;
    let dir = manager.resolve(lock, &spec.name)?;
    let revision_guard = match maki_pack::lock::Lock::acquire_shared(
        &maki_pack::paths::revision_lock(site, &spec.name, &entry.rev),
    ) {
        Ok(guard) => Arc::new(guard),
        Err(error) => {
            failures.push(format!("{}: {}", spec.name, redact_error(&error)));
            return None;
        }
    };
    let requested = match load_requested_permissions(&dir) {
        Ok(requested) => requested,
        Err(problem) => {
            failures.push(format!("{}: {problem}", spec.name));
            return None;
        }
    };
    Some(DiscoveredPackage {
        name: spec.name.clone(),
        dir,
        eager: declared
            .policies
            .values()
            .any(|policy| policy.load.is_eager()),
        requested,
        origin: Origin::Fetched {
            src: spec.src.clone(),
        },
        revision_guard: Some(revision_guard),
    })
}

/// The declared packages that are already installed, at the source and
/// revision the lockfile records, whose leading component has everything it
/// asks for approved.
///
/// The leading component is the first of {components}, in load order, that
/// the package's declaration names, so the agent's wherever the agent loads
/// and `init.lua` declares it, which is the gate a package passed before
/// components existed. Every other component is gated again, by
/// [`eligible`], where its entry loads, and one no file declares is not
/// asked about at all.
///
/// This is what a session still has when it cannot install, so one held lock
/// does not take away the packages the user already had.
fn resolved_on_disk(
    specs: &[&crate::api::pack::Declared],
    site: &Path,
    lock: &maki_pack::lockfile::Lockfile,
    components: &[Component],
    failures: &mut Vec<String>,
) -> Vec<DiscoveredPackage> {
    let manager = maki_pack::manager::Manager::new(site);
    let stores: Vec<(Component, Approvals)> = components
        .iter()
        .map(|&component| {
            let store = read_approvals_file(&approvals_path_in(site, component));
            (component, store.unwrap_or_default())
        })
        .collect();
    let mut found = Vec::new();
    for declared in specs.iter().copied() {
        let Some(package) = resolve_declared(declared, site, &manager, lock, failures) else {
            continue;
        };
        let declared_stores: Vec<&(Component, Approvals)> = stores
            .iter()
            .filter(|(component, _)| declared.policy(*component).is_some())
            .collect();
        for (position, (component, store)) in declared_stores.iter().enumerate() {
            if needs_approval(&package, store, *component) {
                failures.push(approval_required(
                    &package.name,
                    *component,
                    &missing_grants(&package, store, *component),
                ));
                if position == 0 {
                    break;
                }
            }
        }
        if declared_stores
            .first()
            .is_some_and(|(component, store)| eligible(&package, store, *component))
        {
            found.push(package);
        }
    }
    found
}

fn unsaved_approval(component: Component) -> &'static str {
    match component {
        Component::Agent => "permission approval could not be saved",
        Component::Ui => "UI approval could not be saved",
    }
}

fn approval_required(name: &str, component: Component, missing: &[String]) -> String {
    match (component, missing.is_empty()) {
        (Component::Agent, _) => format!(
            "{name}: permission approval is required for {}",
            missing.join(", ")
        ),
        (Component::Ui, true) => format!("{name}: {UI_APPROVAL_REQUIRED}"),
        (Component::Ui, false) => {
            format!("{name}: {UI_APPROVAL_REQUIRED} for {}", missing.join(", "))
        }
    }
}

/// The question that approves {component} of package {name}, which asks for
/// {missing} beyond what is approved.
fn approval_prompt(name: &str, component: Component, missing: &[String]) -> String {
    match component {
        Component::Agent => format!(
            "Allow package {name} these permissions: {}?",
            missing.join(", ")
        ),
        Component::Ui if missing.is_empty() => {
            format!("Run the UI of package {name} with {NO_PERMISSIONS}?")
        }
        Component::Ui => format!(
            "Run the UI of package {name} with these permissions: {}?",
            missing.join(", ")
        ),
    }
}

/// What a package asks for that its approval does not cover.
///
/// Hosts count the same way permissions do: an update that widens `net_hosts`,
/// or drops the list and with it every limit, is asking for reach the user
/// never granted. Registered slugs count the same way: a package that adds a
/// name is asking to receive a key the user never granted it for.
fn missing_requests(requested: &Requested, approved: Option<&Entry>) -> Vec<String> {
    let permissions = approved.map(Entry::permissions).unwrap_or_default();
    let missing_names = requested
        .names()
        .into_iter()
        .filter(|name| !permissions.contains(name));
    let missing_hosts = Reach::requested(requested)
        .beyond(Reach::granted(approved))
        .into_iter()
        .map(|host| format!("{NET_HOST_LABEL} {host}"));
    let approved_providers = approved.map(Entry::providers).unwrap_or_default();
    let missing_providers = requested
        .providers()
        .iter()
        .filter(|slug| !approved_providers.contains(slug))
        .map(|slug| format!("{PROVIDER_TAKEOVER_LABEL} '{slug}'"));
    missing_names
        .chain(missing_hosts)
        .chain(missing_providers)
        .collect()
}

/// How far a package reaches over the network, on one side of an approval.
#[derive(Clone, Copy)]
enum Reach<'a> {
    Offline,
    Hosts(&'a [String]),
    /// `net` with no `net_hosts` list, which is every host.
    Anywhere,
}

impl<'a> Reach<'a> {
    fn of(net: bool, hosts: Option<&'a [String]>) -> Self {
        match (net, hosts) {
            (false, _) => Self::Offline,
            (true, Some(hosts)) => Self::Hosts(hosts),
            (true, None) => Self::Anywhere,
        }
    }

    fn requested(requested: &'a Requested) -> Self {
        Self::of(
            requested.is_requested(Permission::Net),
            requested.net_hosts(),
        )
    }

    fn granted(approved: Option<&'a Entry>) -> Self {
        let net = Permission::Net.manifest_key();
        approved.map_or(Self::Offline, |entry| {
            Self::of(
                entry.permissions().iter().any(|name| name == net),
                entry.net_hosts(),
            )
        })
    }

    /// The hosts `self` reaches and `other` does not. One direction of a
    /// diff, so the review runs it both ways. Going on or off the network at
    /// all is the `net` permission's to report, so only a list that grows, or
    /// gives way to every host, is named here.
    fn beyond(self, other: Self) -> Vec<String> {
        match (self, other) {
            (Self::Offline, _) | (_, Self::Anywhere) | (Self::Anywhere, Self::Offline) => {
                Vec::new()
            }
            (Self::Anywhere, Self::Hosts(_)) => vec![ANY_HOST.to_owned()],
            (Self::Hosts(hosts), Self::Offline) => hosts.to_vec(),
            (Self::Hosts(hosts), Self::Hosts(covered)) => hosts
                .iter()
                .filter(|host| !covered.contains(host))
                .cloned()
                .collect(),
        }
    }
}

/// The declarations that may be installed: the rest name something that
/// already has an owner, and two owners for one name is not resolvable.
fn runnable_declarations<'a>(
    specs: &'a [crate::api::pack::Declared],
    manual: &Discovery,
    report: &mut InstallReport,
    bundled: &Bundled,
) -> Vec<&'a crate::api::pack::Declared> {
    // Includes names discovery refused, so a manual package maki could not
    // read still owns its name rather than being silently overwritten.
    let manual_names = manual.known_names();
    specs
        .iter()
        .filter(|declared| {
            let name = &declared.spec.name;
            if let Some(failure) = owner_conflict(name, bundled) {
                report.failures.push(failure);
                return false;
            }
            if manual_names.iter().any(|manual| manual == name) {
                let path = manual
                    .packages
                    .iter()
                    .find(|package| package.name == *name)
                    .map(|package| format!(" at {}", package.dir.display()))
                    .unwrap_or_default();
                report.failures.push(format!(
                    "{name}: managed package name conflicts with manual package{path}"
                ));
                return false;
            }
            true
        })
        .collect()
}

/// One component's approval store while an install pass decides.
struct PassStore {
    component: Component,
    store: Approvals,
    changed: bool,
    newly_approved: BTreeSet<String>,
}

/// Decides which installed packages may load, asking {confirm} about each
/// component of {components} a file declares that the store has not
/// already approved.
///
/// A package whose leading component (see [`resolved_on_disk`]) is refused
/// is not loaded, so nothing else about it is asked.
fn grant_installed(
    specs: &[&crate::api::pack::Declared],
    site: &Path,
    manager: &maki_pack::manager::Manager,
    lock: &maki_pack::lockfile::Lockfile,
    components: &[Component],
    confirm: &mut dyn FnMut(&str) -> bool,
    report: &mut InstallReport,
) {
    let mut stores = Vec::with_capacity(components.len());
    let mut unreadable = Vec::new();
    for (position, &component) in components.iter().enumerate() {
        let Some(store) = read_approvals_file(&approvals_path_in(site, component)) else {
            if position == 0 {
                report.failures.push(
                    "the package approval store is unreadable, so no package was loaded".to_owned(),
                );
                return;
            }
            // Left out of the pass, so nothing is asked or written for this
            // component, and its entries stay ineligible where they load. Only
            // a package that declares it is told, so the file never touches
            // one that has no such component.
            unreadable.push(component);
            continue;
        };
        stores.push(PassStore {
            component,
            store,
            changed: false,
            newly_approved: BTreeSet::new(),
        });
    }

    for declared in specs.iter().copied() {
        let Some(package) = resolve_declared(declared, site, manager, lock, &mut report.failures)
        else {
            continue;
        };
        let name = &package.name;
        let key = ApprovalKey::new(name.clone(), &declared.spec.src);
        if unreadable.iter().any(|&component| {
            package.requested.declares(component) && declared.policy(component).is_some()
        }) {
            report
                .failures
                .push(format!("{name}: {UI_APPROVAL_STORE_UNREADABLE}"));
        }
        // An entry under this name that the key did not match belongs to
        // some other source, so it is dropped rather than left to be
        // inherited, from each component's store on its own and before any
        // answer, so a refusal cannot leave one behind.
        for pass in &mut stores {
            if pass.store.get(&key).is_none() {
                pass.changed |= pass.store.revoke(name);
            }
        }
        // A package stands on its leading component, so one whose leading
        // store could not be read, or that declares nothing this host
        // loads, is not let in on anyone's word.
        let leading = declared.components_in(components).next();
        if leading.is_none_or(|leading| unreadable.contains(&leading)) {
            continue;
        }
        let mut admitted = true;
        let declared_passes = stores
            .iter_mut()
            .filter(|pass| declared.policy(pass.component).is_some());
        for (position, pass) in declared_passes.enumerate() {
            if !needs_approval(&package, &pass.store, pass.component) {
                continue;
            }
            let missing = missing_grants(&package, &pass.store, pass.component);
            if !confirm(&approval_prompt(name, pass.component, &missing)) {
                report
                    .failures
                    .push(approval_required(name, pass.component, &missing));
                if position == 0 {
                    admitted = false;
                    break;
                }
                continue;
            }
            PendingGrant::new(pass.component, package.requested.of(pass.component))
                .record(&mut pass.store, &key);
            pass.changed = true;
            pass.newly_approved.insert(name.clone());
        }
        if admitted {
            report.packages.push(package);
        }
    }

    for pass in &stores {
        if !pass.changed
            || write_approvals_at(&approvals_path_in(site, pass.component), &pass.store)
        {
            continue;
        }
        report.failures.extend(
            pass.newly_approved
                .iter()
                .map(|name| format!("{name}: {}", unsaved_approval(pass.component))),
        );
        // An approval that did not land does not let its package in on the
        // leading component's word; any other component is gated again,
        // from the store on disk, where its entry loads.
        report.packages.retain(|package| {
            !pass.newly_approved.contains(&package.name)
                || specs
                    .iter()
                    .find(|declared| declared.spec.name == package.name)
                    .and_then(|declared| declared.components_in(components).next())
                    != Some(pass.component)
        });
    }
}

/// The declarations of {runnable} a host that loads {components} may act
/// on: one naming a package that another component also owns, from another
/// source, would replace that component's checkout, which only a host that
/// loads every owner may do, so it is reported and left out.
fn owned_declarations<'a>(
    runnable: Vec<&'a crate::api::pack::Declared>,
    lock: &Lockfile,
    components: &[Component],
    report: &mut InstallReport,
) -> Vec<&'a crate::api::pack::Declared> {
    runnable
        .into_iter()
        .filter(|declared| {
            let name = &declared.spec.name;
            let foreign = lock
                .get(name)
                .is_some_and(|entry| entry.src != declared.spec.src)
                && !owns_all(lock, name, components);
            if foreign {
                report
                    .failures
                    .push(format!("{name}: {FOREIGN_SOURCE_CONFLICT}"));
            }
            !foreign
        })
        .collect()
}

/// Installs the packages the global `init.lua` and `ui.lua` declared and
/// reports where each one landed.
///
/// Runs on the caller's thread, never the Lua thread: installing clones, and
/// loading blocks on a reply from the runtime, so doing either from inside a
/// Lua call would wait on a message that cannot be processed until it returns.
/// One unreachable repository is reported and skipped so a network problem
/// does not stop Maki from starting.
///
/// Only {components}, the ones the calling host loads, are asked about: an
/// agent host never asks to approve UI code.
pub fn install_declared(
    specs: &[crate::api::pack::Declared],
    interaction: Interaction,
    components: &[Component],
    bundled: &Bundled,
) -> InstallReport {
    match PackRoots::of_user() {
        Ok(roots) => install_declared_in(&roots, specs, interaction, components, bundled),
        Err(error) => InstallReport {
            failures: vec![format!(
                "no data directory, so no package was installed: {error}"
            )],
            ..InstallReport::default()
        },
    }
}

/// Where a host's packages live: the site holding the checkouts and
/// approvals, and the lockfile recording what is installed.
#[derive(Debug, Clone)]
pub struct PackRoots {
    pub site: PathBuf,
    pub lockfile: Option<PathBuf>,
}

impl PackRoots {
    /// The user's own: [`site_dir`] and [`lockfile_path`].
    pub fn of_user() -> io::Result<Self> {
        Ok(Self {
            site: site_dir()?,
            lockfile: lockfile_path(),
        })
    }
}

/// As [`install_declared`], against the site and lockfile of {roots}.
pub fn install_declared_in(
    roots: &PackRoots,
    specs: &[crate::api::pack::Declared],
    interaction: Interaction,
    components: &[Component],
    bundled: &Bundled,
) -> InstallReport {
    let mut report = InstallReport::default();
    let site = &roots.site;
    let lock_path = roots.lockfile.as_deref();

    // Held across the whole read, install, and write. Atomic rename gives
    // durability but not isolation: without this, two processes could each read
    // the same lockfile and the second write would discard the first's entries.
    let _guard = match lock_path.map(maki_pack::paths::sidecar_lock) {
        Some(path) => match maki_pack::lock::Lock::acquire_retrying(&path) {
            Ok(guard) => Some(guard),
            Err(e) => {
                // The kernel releases the lock when its process exits,
                // including after a crash.
                tracing::error!(error = %e, "could not take the package lock");
                report.failures.push(redact_error(&e));
                // Installing needs the lock; reading what is already there
                // does not. Returning nothing would tell the session it has
                // no managed packages at all, because discovery skips the
                // managed group, so a second maki started during a clone
                // would come up with every package the user has missing.
                if let Some(lock) = read_lockfile(lock_path) {
                    let manual = discover(site, bundled);
                    let runnable = runnable_declarations(specs, &manual, &mut report, bundled);
                    report.packages =
                        resolved_on_disk(&runnable, site, &lock, components, &mut report.failures);
                }
                return report;
            }
        },
        None => None,
    };

    let mut lock = match read_lockfile(lock_path) {
        Some(lock) => lock,
        None => {
            report
                .failures
                .push("the pack lockfile is unreadable, so no package was installed".to_owned());
            return report;
        }
    };

    let manager = maki_pack::manager::Manager::new(site);
    for error in manager.prune(&lock) {
        tracing::warn!(error = %error, "could not prune a stale package revision");
        report.failures.push(redact_error(&error));
    }
    if specs.is_empty() {
        return report;
    }
    let manual = discover(site, bundled);
    let runnable = runnable_declarations(specs, &manual, &mut report, bundled);
    let runnable = owned_declarations(runnable, &lock, components, &mut report);
    let mut changed = false;

    // Asked once for the whole batch, before anything is cloned. A package
    // already recorded at the revision it asks for is not a new trust
    // decision, so only a fresh source prompts. Credentials are redacted,
    // because the prompt is the one place a source is shown in full.
    let new_sources: Vec<String> = runnable
        .iter()
        .copied()
        .filter(|declared| {
            declared.confirm() && !installed_from_declared_source(declared, &lock, &manager)
        })
        .map(|declared| {
            format!(
                "{} from {}",
                declared.spec.name,
                maki_pack::git::redact(&declared.spec.src)
            )
        })
        .collect();
    let confirmed = new_sources.is_empty()
        || interaction.confirm(&format!(
            "Install these packages?\n  {}",
            new_sources.join("\n  ")
        ));

    for declared in runnable.iter().copied() {
        let spec = &declared.spec;
        if declared.confirm()
            && !installed_from_declared_source(declared, &lock, &manager)
            && !confirmed
        {
            report.failures.push(format!(
                "{}: installation requires confirmation; set confirm = false for a non-interactive install",
                spec.name
            ));
            continue;
        }
        let fresh = lock.get(&spec.name).is_none();
        let result = match Handle::current().block_on(manager.ensure_installed(spec, &mut lock)) {
            Ok(result) => result,
            Err(e) => {
                let message = redact_error(&e);
                tracing::error!(package = %spec.name, error = %message, "failed to install package");
                report
                    .failures
                    .push(format!("{}: failed to install: {message}", spec.name));
                continue;
            }
        };
        changed |= result.changed;
        // The components this host loads that a file declares, so ownership
        // is durable without running the other side's configuration.
        let owners = owners_of(declared.components_in(components));
        changed |= if fresh {
            lock.set_owners(&spec.name, &owners)
        } else {
            lock.add_owners(&spec.name, &owners)
        };
    }

    // Written once, after the installs, and only when something moved.
    if changed {
        let recorded = lock_path.is_some_and(|path| write_lockfile(path, &lock));
        if !recorded {
            // The packages are on disk but nothing records where. The next
            // start reads the old lockfile and would not find them, so they
            // must not be reported as installed either.
            report.packages.clear();
            report
                .failures
                .push("the pack lockfile could not be written, so no package was used".to_owned());
            return report;
        }
    }

    grant_installed(
        &runnable,
        site,
        &manager,
        &lock,
        components,
        &mut |prompt| interaction.confirm(prompt),
        &mut report,
    );
    report
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackReport {
    pub updated: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub failures: Vec<String>,
}

impl PackReport {
    pub fn changed(&self) -> bool {
        !self.updated.is_empty() || !self.removed.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.updated.is_empty() {
            parts.push(format!("{} updated", self.updated.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("{} removed", self.removed.len()));
        }
        if parts.is_empty() {
            "No package changes".to_owned()
        } else {
            parts.join(", ")
        }
    }

    pub fn message(&self) -> String {
        if self.failures.is_empty() {
            self.summary()
        } else {
            self.failures.join("; ")
        }
    }
}

pub fn installed_names() -> Option<Vec<String>> {
    read_lockfile(lockfile_path().as_deref())
        .map(|lock| lock.install_order().map(str::to_owned).collect())
}

/// A parsed package command. Parsing already turned away the malformed shapes,
/// so planning only has to answer questions about the packages themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackCommand {
    Update {
        /// `None` means every declared package that is installed.
        name: Option<String>,
        options: UpdateOptions,
    },
    Delete {
        target: DeleteTarget,
        force: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    /// Every installed package no longer declared with `maki.pack.add`.
    Undeclared,
    One(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    pub force: bool,
    pub target: UpdateTarget,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateTarget {
    #[default]
    Version,
    Lockfile,
}

/// One package a validated command decided to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOp {
    Update {
        name: String,
        options: UpdateOptions,
    },
    Delete {
        name: String,
        force: bool,
    },
}

impl PackCommand {
    pub fn parse(name: &str, args: &str, bang: bool) -> Result<Self, String> {
        let mut names = Vec::new();
        let mut lockfile = false;
        let mut all = false;
        for word in args.split_whitespace() {
            match word {
                "++lockfile" => lockfile = true,
                "++all" => all = true,
                flag if flag.starts_with('+') || flag.starts_with('-') => {
                    return Err(format!("{name}: unknown option {flag:?}"));
                }
                other => names.push(other.to_owned()),
            }
        }

        match name {
            "/packupdate" => {
                if all {
                    return Err("/packupdate: ++all is not an option".to_owned());
                }
                if names.len() > 1 {
                    return Err(UPDATE_USAGE.to_owned());
                }
                Ok(Self::Update {
                    name: names.pop(),
                    options: UpdateOptions {
                        force: bang,
                        target: if lockfile {
                            UpdateTarget::Lockfile
                        } else {
                            UpdateTarget::Version
                        },
                    },
                })
            }
            "/packdel" => {
                if lockfile {
                    return Err("/packdel: ++lockfile applies only to /packupdate".to_owned());
                }
                let target = match (all, names.len()) {
                    (true, 0) => DeleteTarget::Undeclared,
                    (false, 1) => DeleteTarget::One(names.remove(0)),
                    _ => return Err(DELETE_USAGE.to_owned()),
                };
                Ok(Self::Delete {
                    target,
                    force: bang,
                })
            }
            other => Err(format!("{other}: not a package command")),
        }
    }
}

pub fn plan_command(
    command: &PackCommand,
    declared: &[crate::api::pack::Declared],
    installed: &[String],
) -> Result<Vec<PlannedOp>, String> {
    let is_declared = |name: &str| declared.iter().any(|declared| declared.spec.name == *name);
    match command {
        PackCommand::Update { name, options } => {
            let names = match name {
                Some(name) => {
                    if !is_declared(name) {
                        return Err(format!("{name}: not a package declared with maki.pack.add"));
                    }
                    if !installed.contains(name) {
                        return Err(format!("{name}: not installed, so it cannot be updated"));
                    }
                    vec![name.clone()]
                }
                None => declared
                    .iter()
                    .map(|declared| declared.spec.name.clone())
                    .filter(|name| installed.contains(name))
                    .collect(),
            };
            if names.is_empty() {
                return Err("no declared package is installed; nothing was updated".to_owned());
            }
            Ok(names
                .into_iter()
                .map(|name| PlannedOp::Update {
                    name,
                    options: *options,
                })
                .collect())
        }
        PackCommand::Delete { target, force } => {
            let names = match target {
                DeleteTarget::One(name) => {
                    if !installed.contains(name) {
                        return Err(format!("{name}: not an installed package"));
                    }
                    if is_declared(name) {
                        return Err(format!("{name}: {DECLARED_DELETE_REFUSAL}"));
                    }
                    vec![name.clone()]
                }
                DeleteTarget::Undeclared => installed
                    .iter()
                    .filter(|name| !is_declared(name))
                    .cloned()
                    .collect(),
            };
            if names.is_empty() {
                return Err("no undeclared package matched; nothing was removed".to_owned());
            }
            Ok(names
                .into_iter()
                .map(|name| PlannedOp::Delete {
                    name,
                    force: *force,
                })
                .collect())
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackContext {
    declared: Vec<crate::api::pack::Declared>,
    installed: Vec<String>,
    active: BTreeSet<String>,
}

impl PackContext {
    /// Combines runtime ownership for a native package review. A name must
    /// still identify the same source and version in both init files.
    pub fn merge(mut self, other: Self) -> Result<Self, String> {
        for declared in other.declared {
            match self
                .declared
                .iter_mut()
                .find(|entry| entry.spec.name == declared.spec.name)
            {
                Some(entry) if entry.spec != declared.spec => {
                    return Err(format!(
                        "{}: {CONFLICTING_DECLARATIONS}",
                        declared.spec.name
                    ));
                }
                Some(entry) => entry.policies.extend(declared.policies),
                None => self.declared.push(declared),
            }
        }
        for name in other.installed {
            if !self.installed.contains(&name) {
                self.installed.push(name);
            }
        }
        self.active.extend(other.active);
        Ok(self)
    }

    pub fn new(
        declared: Vec<crate::api::pack::Declared>,
        installed: Vec<String>,
        active: BTreeSet<String>,
    ) -> Self {
        Self {
            declared,
            installed,
            active,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackPreparation {
    Complete(PackReport),
    Ready(PackPlan),
    Review { prompt: String, plan: PackPlan },
}

impl PackPreparation {
    pub fn failed(message: String) -> Self {
        Self::Complete(PackReport {
            failures: vec![message],
            ..PackReport::default()
        })
    }
}

/// Only preparation builds a plan worth applying. The `Default` one carries no
/// operations, so tests can open the review overlay with no git repository
/// behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackPlan {
    operations: Vec<PreparedPackOp>,
    site: PathBuf,
    lock_path: PathBuf,
    /// The components whose approvals the plan reviews and revokes: the
    /// ones the host that prepared it loads.
    components: &'static [Component],
    report: PackReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedPackOp {
    Update {
        name: String,
        update: Box<maki_pack::manager::PreparedUpdate>,
        /// Permissions the review showed, saved once the user accepts so
        /// startup has nothing left to ask. Stays `None` for a forced update:
        /// force skips the review, and an unreviewed manifest may not grant
        /// itself anything.
        approve: Option<PendingApproval>,
    },
    Delete {
        name: String,
        /// The components the checkout's manifest declares, whose
        /// approvals must be revoked; every one when it cannot be read.
        declared: Vec<Component>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingApproval {
    src: String,
    /// One grant per component the review showed.
    grants: Vec<PendingGrant>,
}

/// What one component is approved for, as its manifest asked when the user
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingGrant {
    component: Component,
    names: Vec<String>,
    net_hosts: Option<Vec<String>>,
    providers: Vec<String>,
}

impl PendingGrant {
    fn new(component: Component, requested: &Requested) -> Self {
        Self {
            component,
            names: requested.names(),
            net_hosts: requested.net_hosts().map(<[String]>::to_vec),
            providers: requested.providers().to_vec(),
        }
    }

    fn record(self, store: &mut Approvals, key: &ApprovalKey) {
        store.approve(key, self.names, self.net_hosts, self.providers);
    }
}

/// Each component's approval store over one plan: read at most once, so a
/// batch touching several packages cannot lose an earlier edit by reloading
/// a file mid-run, and written once, only where something changed.
struct PlanStores<'a> {
    site: &'a Path,
    /// `None` for a store that exists but cannot be trusted.
    stores: Vec<(Component, Option<Approvals>)>,
    /// Failures that only count if their component's single write fails.
    /// The list is also the dirty flag: nothing in it, nothing to write.
    unsaved: Vec<(Component, String)>,
}

impl<'a> PlanStores<'a> {
    fn new(site: &'a Path) -> Self {
        Self {
            site,
            stores: Vec::new(),
            unsaved: Vec::new(),
        }
    }

    fn get(&mut self, component: Component) -> Option<&mut Approvals> {
        let position = match self.stores.iter().position(|(c, _)| *c == component) {
            Some(position) => position,
            None => {
                let store = read_approvals_file(&approvals_path_in(self.site, component));
                self.stores.push((component, store));
                self.stores.len() - 1
            }
        };
        self.stores[position].1.as_mut()
    }

    /// Writes every store an edit changed, adding to {failures} the edits of
    /// any store whose write did not land.
    fn finish(self, failures: &mut Vec<String>) {
        for (component, store) in &self.stores {
            let Some(store) = store else { continue };
            let mut pending = self
                .unsaved
                .iter()
                .filter(|(edited, _)| edited == component)
                .map(|(_, failure)| failure.clone())
                .peekable();
            if pending.peek().is_some()
                && !write_approvals_at(&approvals_path_in(self.site, *component), store)
            {
                failures.extend(pending);
            }
        }
    }
}

fn save_failure(component: Component) -> &'static str {
    match component {
        Component::Agent => SAVE_APPROVAL_FAILURE,
        Component::Ui => SAVE_UI_APPROVAL_FAILURE,
    }
}

fn revoke_failure(component: Component) -> &'static str {
    match component {
        Component::Agent => REVOKE_APPROVAL_FAILURE,
        Component::Ui => REVOKE_UI_APPROVAL_FAILURE,
    }
}

/// Prepares {command} for a host that loads {components}, which are the
/// only ones whose approvals it reviews and revokes.
pub fn prepare_pack_command(
    command: &PackCommand,
    context: &PackContext,
    components: &'static [Component],
    bundled: &Bundled,
) -> PackPreparation {
    let site = match site_dir() {
        Ok(site) => site,
        Err(error) => {
            return PackPreparation::failed(format!(
                "no data directory, so packages cannot be changed: {error}"
            ));
        }
    };
    let Some(lock_path) = lockfile_path() else {
        return PackPreparation::failed(
            "no config directory, so packages cannot be changed".to_owned(),
        );
    };
    let operations = match plan_owned(command, context, components, &lock_path) {
        Ok(operations) => operations,
        Err(error) => return PackPreparation::failed(error),
    };
    prepare_pack_ops_at(
        &operations,
        &context.declared,
        &context.active,
        components,
        &site,
        &lock_path,
        bundled,
    )
}

/// Plans {command} over the installed packages a host that loads
/// {components} owns a part of, as the lockfile at {lock_path} records
/// them: the only ones its commands act on.
fn plan_owned(
    command: &PackCommand,
    context: &PackContext,
    components: &[Component],
    lock_path: &Path,
) -> Result<Vec<PlannedOp>, String> {
    let lock = read_lockfile(Some(lock_path))
        .ok_or_else(|| "the pack lockfile is unreadable, so nothing was changed".to_owned())?;
    let owned: Vec<String> = context
        .installed
        .iter()
        .filter(|name| owns_any(&lock, name, components))
        .cloned()
        .collect();
    if let PackCommand::Delete {
        target: DeleteTarget::One(name),
        ..
    } = command
        && context.installed.contains(name)
        && !owned.contains(name)
    {
        return Err(format!("{name}: {UNOWNED_DELETE_REFUSAL}"));
    }
    plan_command(command, &context.declared, &owned)
}

fn prepare_pack_ops_at(
    ops: &[PlannedOp],
    declared: &[crate::api::pack::Declared],
    active: &BTreeSet<String>,
    components: &'static [Component],
    site: &Path,
    lock_path: &Path,
    bundled: &Bundled,
) -> PackPreparation {
    let mut report = PackReport::default();
    let guard =
        match maki_pack::lock::Lock::acquire_retrying(&maki_pack::paths::sidecar_lock(lock_path)) {
            Ok(guard) => guard,
            Err(error) => {
                report.failures.push(redact_error(&error));
                return PackPreparation::Complete(report);
            }
        };
    let lock = match read_lockfile(Some(lock_path)) {
        Some(lock) => lock,
        None => {
            report
                .failures
                .push("the pack lockfile is unreadable, so nothing was changed".to_owned());
            return PackPreparation::Complete(report);
        }
    };
    // Fetching can sit on the network for a long time and changes nothing on
    // disk, so holding the sidecar lock through it would block every other maki
    // for no gain. `apply_pack_plan` takes it again and rereads the lockfile
    // before it writes anything.
    drop(guard);
    let manager = maki_pack::manager::Manager::new(site);
    let manual_names = discover(site, bundled).known_names();
    let mut prepared = Vec::new();
    let mut review = Vec::new();
    let mut stores = PlanStores::new(site);

    for op in ops {
        match op {
            PlannedOp::Update { name, options } => {
                if owner_conflict(name, bundled).is_some() || manual_names.contains(name) {
                    report
                        .failures
                        .push(format!("{name}: {OWNER_CONFLICT_FAILURE}"));
                    continue;
                }
                let Some(declaration) =
                    declared.iter().find(|declared| declared.spec.name == *name)
                else {
                    report.failures.push(format!(
                        "{name}: not declared with maki.pack.add, so it cannot be updated"
                    ));
                    continue;
                };
                if !owns_all(&lock, name, components) {
                    report
                        .failures
                        .push(format!("{name}: {SHARED_UPDATE_REFUSAL}"));
                    continue;
                }
                let spec = &declaration.spec;
                // Only the components a file declares are reviewed, since
                // no other one loads.
                let reviewed_components: Vec<Component> =
                    declaration.components_in(components).collect();
                let key = ApprovalKey::new(name, &spec.src);
                // `Some` exactly when this update needs a review, and each
                // component's baseline the review diffs the new manifest
                // against, `None` for a component never approved.
                // The leading component's store is read before the git work,
                // so an untrustworthy one stops the update before it fetches.
                // Any other store is read only for a package whose new
                // manifest declares that component.
                let reviewed = !options.force;
                if reviewed
                    && let Some(&lead) = reviewed_components.first()
                    && stores.get(lead).is_none()
                {
                    report.failures.push(format!(
                        "{name}: the pack approval store is unreadable, so the update cannot be reviewed"
                    ));
                    continue;
                }
                let proposal = match Handle::current().block_on(manager.prepare_update(
                    spec,
                    &lock,
                    options.target == UpdateTarget::Lockfile,
                )) {
                    Ok(proposal) => proposal,
                    Err(error) => {
                        report
                            .failures
                            .push(format!("{name}: {}", redact_error(&error)));
                        continue;
                    }
                };
                let requested = match proposed_permissions(name, &proposal) {
                    Ok(requested) => requested,
                    Err(error) => {
                        report
                            .failures
                            .push(format!("{name}: {}", redact_error(&error)));
                        continue;
                    }
                };
                if !proposal.changed {
                    continue;
                }
                let mut approve = None;
                if reviewed {
                    // ACP rechecks this review before applying it, so the revisions must be exact.
                    let mut lines = format!("{name}: {} -> {}", proposal.old_rev, proposal.new_rev);
                    let mut grants = Vec::new();
                    for &component in &reviewed_components {
                        if !requested.declares(component) {
                            continue;
                        }
                        let Some(store) = stores.get(component) else {
                            report
                                .failures
                                .push(format!("{name}: {UI_APPROVAL_STORE_UNREADABLE}"));
                            continue;
                        };
                        let baseline = store.get(&key).cloned();
                        let wanted = requested.of(component);
                        lines.push_str("\n  ");
                        lines.push_str(&component_review(component, wanted, baseline.as_ref()));
                        grants.push(PendingGrant::new(component, wanted));
                    }
                    if !proposal.old_rev_available {
                        lines.push_str(UNAVAILABLE_OLD_REVISION);
                    }
                    review.push(lines);
                    approve = Some(PendingApproval {
                        src: spec.src.clone(),
                        grants,
                    });
                }
                prepared.push(PreparedPackOp::Update {
                    name: name.clone(),
                    update: Box::new(proposal),
                    approve,
                });
            }
            PlannedOp::Delete { name, force } => {
                if declared.iter().any(|declared| declared.spec.name == *name) {
                    report
                        .failures
                        .push(format!("{name}: {DECLARED_DELETE_REFUSAL}"));
                    continue;
                }
                if active.contains(name) {
                    report
                        .failures
                        .push(format!("{name}: {ACTIVE_DELETE_REFUSAL}"));
                    continue;
                }
                if lock.get(name).is_none() {
                    report
                        .failures
                        .push(format!("{name}: not an installed managed package"));
                    continue;
                }
                if !owns_any(&lock, name, components) {
                    report
                        .failures
                        .push(format!("{name}: {UNOWNED_DELETE_REFUSAL}"));
                    continue;
                }
                if !force {
                    let line = if owns_all(&lock, name, components) {
                        REVIEW_DELETE_LINE
                    } else {
                        REVIEW_RELEASE_LINE
                    };
                    review.push(format!("{name}: {line}"));
                }
                let requests = manager
                    .resolve(&lock, name)
                    .and_then(|dir| load_requested_permissions(&dir).ok());
                prepared.push(PreparedPackOp::Delete {
                    name: name.clone(),
                    declared: components
                        .iter()
                        .copied()
                        .filter(|&component| {
                            requests
                                .as_ref()
                                .is_none_or(|requests| requests.declares(component))
                        })
                        .collect(),
                });
            }
        }
    }

    if prepared.is_empty() {
        return PackPreparation::Complete(report);
    }
    let plan = PackPlan {
        operations: prepared,
        site: site.to_path_buf(),
        lock_path: lock_path.to_path_buf(),
        components,
        report,
    };
    if review.is_empty() {
        PackPreparation::Ready(plan)
    } else {
        PackPreparation::Review {
            prompt: format!("{REVIEW_PROMPT_HEADER}\n  {}", review.join("\n  ")),
            plan,
        }
    }
}

/// The update review's line for {component}, which now asks for {wanted}
/// and was approved for {baseline}.
fn component_review(component: Component, wanted: &Requested, baseline: Option<&Entry>) -> String {
    match (component, baseline) {
        (Component::Agent, baseline) => format!(
            "permissions: {}",
            update_permission_diff(wanted, &baseline.cloned().unwrap_or_default())
        ),
        (Component::Ui, Some(baseline)) => format!(
            "{UI_REVIEW_LABEL}: {}",
            update_permission_diff(wanted, baseline)
        ),
        (Component::Ui, None) => {
            let asks = missing_requests(wanted, None);
            let asks = if asks.is_empty() {
                NO_PERMISSIONS.to_owned()
            } else {
                asks.join(", ")
            };
            format!("{UI_REVIEW_LABEL} ({UI_REVIEW_NEW}): {asks}")
        }
    }
}

pub fn apply_pack_plan(plan: PackPlan) -> PackReport {
    let PackPlan {
        operations,
        site,
        lock_path,
        components,
        mut report,
    } = plan;
    let _guard = match maki_pack::lock::Lock::acquire_retrying(&maki_pack::paths::sidecar_lock(
        &lock_path,
    )) {
        Ok(guard) => guard,
        Err(error) => {
            report.failures.push(redact_error(&error));
            return report;
        }
    };
    let mut lock = match read_lockfile(Some(&lock_path)) {
        Some(lock) => lock,
        None => {
            report
                .failures
                .push("the pack lockfile is unreadable, so nothing was changed".to_owned());
            return report;
        }
    };
    let manager = maki_pack::manager::Manager::new(&site);
    let mut stores = PlanStores::new(&site);

    for operation in operations {
        match operation {
            PreparedPackOp::Update { name, .. } if !owns_all(&lock, &name, components) => report
                .failures
                .push(format!("{name}: {SHARED_UPDATE_REFUSAL}")),
            PreparedPackOp::Update {
                name,
                update,
                approve,
            } => match Handle::current().block_on(manager.apply_update(&update, &mut lock)) {
                Ok(installed) => {
                    report.updated.push((name.clone(), installed.rev));
                    let Some(approve) = approve else { continue };
                    let key = ApprovalKey::new(&name, &approve.src);
                    for grant in approve.grants {
                        let component = grant.component;
                        let failure = format!("{name}: {}", save_failure(component));
                        match stores.get(component) {
                            Some(store) => {
                                grant.record(store, &key);
                                stores.unsaved.push((component, failure));
                            }
                            None => report.failures.push(failure),
                        }
                    }
                }
                Err(error) => report
                    .failures
                    .push(format!("{name}: {}", redact_error(&error))),
            },
            PreparedPackOp::Delete { name, .. } if !owns_any(&lock, &name, components) => report
                .failures
                .push(format!("{name}: {UNOWNED_DELETE_REFUSAL}")),
            PreparedPackOp::Delete { name, declared } => {
                match release(&manager, &mut lock, &name, components) {
                    Ok(()) => {
                        report.removed.push(name.clone());
                        for &component in components {
                            let failure = format!("{name}: {}", revoke_failure(component));
                            match stores.get(component) {
                                Some(store) => {
                                    if store.revoke(&name) {
                                        stores.unsaved.push((component, failure));
                                    }
                                }
                                // A store that cannot be read touches only the
                                // packages that declare its component.
                                None if declared.contains(&component) => {
                                    report.failures.push(failure)
                                }
                                None => {}
                            }
                        }
                    }
                    Err(error) => report
                        .failures
                        .push(format!("{name}: {}", redact_error(&error))),
                }
            }
        }
    }

    stores.finish(&mut report.failures);

    if report.changed() {
        if !write_lockfile(&lock_path, &lock) {
            report
                .failures
                .push("packages changed but the lockfile could not be written".to_owned());
        } else {
            for error in manager.prune(&lock) {
                report.failures.push(redact_error(&error));
            }
        }
    }
    report
}

/// Takes the owners of {components} off {name}, removing the checkout and
/// its lock entry only when no owner is left, so a host that loads one
/// component never removes what the other still loads.
fn release(
    manager: &maki_pack::manager::Manager,
    lock: &mut Lockfile,
    name: &str,
    components: &[Component],
) -> Result<(), maki_pack::manager::ManagerError> {
    if owns_all(lock, name, components) {
        return manager.remove(name, lock);
    }
    lock.remove_owners(name, &owners_of(components.iter().copied()));
    Ok(())
}

fn proposed_permissions(
    name: &str,
    update: &maki_pack::manager::PreparedUpdate,
) -> Result<PackageRequests, PluginError> {
    let Some(manifest) = update.new_manifest.as_deref() else {
        return Ok(PackageRequests::none());
    };
    // The new revision is not on disk yet, so this path is only a label that
    // tells the user which manifest failed to parse.
    let path = PathBuf::from(format!("{name}@{}", update.new_rev)).join(PLUGIN_MANIFEST);
    requested_permissions_from_text(manifest, &path)
}

fn update_permission_diff(requested: &Requested, approved: &Entry) -> String {
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    for permission in Permission::ALL {
        let is_approved = approved
            .permissions()
            .iter()
            .any(|name| name == permission.manifest_key());
        match (is_approved, requested.is_requested(*permission)) {
            (false, true) => additions.push(format!("+{permission}")),
            (true, false) => removals.push(format!("-{permission}")),
            _ => {}
        }
    }
    let wanted = Reach::requested(requested);
    let granted = Reach::granted(Some(approved));
    additions.extend(
        wanted
            .beyond(granted)
            .into_iter()
            .map(|host| format!("+{NET_HOST_LABEL} {host}")),
    );
    removals.extend(
        granted
            .beyond(wanted)
            .into_iter()
            .map(|host| format!("-{NET_HOST_LABEL} {host}")),
    );
    for slug in requested.providers() {
        if !approved.providers().contains(slug) {
            additions.push(format!("+{PROVIDER_TAKEOVER_LABEL} '{slug}'"));
        }
    }
    for slug in approved.providers() {
        if !requested.providers().contains(slug) {
            removals.push(format!("-{PROVIDER_TAKEOVER_LABEL} '{slug}'"));
        }
    }
    additions.extend(removals);
    if additions.is_empty() {
        "unchanged".to_owned()
    } else {
        additions.join(", ")
    }
}

/// The reason a managed package may not own a name, if something else does.
fn owner_conflict(name: &str, bundled: &Bundled) -> Option<String> {
    bundled
        .is_bundled(name)
        .then(|| format!("{name}: managed package name conflicts with a builtin plugin"))
}

fn redact_error(error: &impl std::fmt::Display) -> String {
    sanitize_message(&maki_pack::git::redact(&error.to_string()))
}

pub(crate) fn read_lockfile(path: Option<&Path>) -> Option<maki_pack::lockfile::Lockfile> {
    let Some(path) = path else {
        return Some(maki_pack::lockfile::Lockfile::default());
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(maki_pack::lockfile::Lockfile::default());
        }
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "pack lockfile could not be read");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(lock) => Some(lock),
        Err(error) => {
            tracing::error!(%error, "pack lockfile is unreadable; refusing to change packages");
            None
        }
    }
}

fn write_lockfile(path: &Path, lock: &maki_pack::lockfile::Lockfile) -> bool {
    write_json(path, lock, "pack lockfile")
}

fn write_approvals_at(path: &Path, approvals: &Approvals) -> bool {
    write_json(path, approvals, "pack approvals")
}

/// Replaces a shared JSON file in one step.
///
/// Returns whether the write landed rather than an error: every caller has to
/// keep going either way, and the reason belongs in the log, not in a message
/// each of them would have to format again.
fn write_json(path: &Path, value: &impl serde::Serialize, what: &str) -> bool {
    let written = serde_json::to_string_pretty(value)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            maki_storage::atomic_write(path, text.as_bytes()).map_err(|error| error.to_string())
        });
    match written {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "failed to write {}", what);
            false
        }
    }
}

/// Something the walk could not use, and the name it had for it.
///
/// One record and not two lists, because the name and the reason are two
/// halves of one fact and the config layer needs the name: a `plugins.<name>`
/// table naming a package that failed to load still names something real.
#[derive(Debug)]
pub struct Problem {
    /// `None` when the failure belongs to no single package, such as a group
    /// directory that could not be read at all.
    pub name: Option<String>,
    pub error: PluginError,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

/// What a discovery walk found, and what it had to refuse.
///
/// Problems are collected rather than returned as one error, because one
/// unusable package must not stop the others from loading.
#[derive(Debug, Default)]
pub struct Discovery {
    pub packages: Vec<DiscoveredPackage>,
    pub problems: Vec<Problem>,
}

impl Discovery {
    /// Records a package that was found but cannot load.
    fn refuse(&mut self, name: String, error: PluginError) {
        self.problems.push(Problem {
            name: Some(name),
            error,
        });
    }

    /// Records a failure that names no package.
    fn note(&mut self, error: PluginError) {
        self.problems.push(Problem { name: None, error });
    }

    /// Every package name the walk saw, loadable or not.
    ///
    /// This is what a `plugins.<name>` table is validated against. Validating
    /// against the loadable set instead would turn a package maki itself
    /// refused into a config error, and blame the user's config for it.
    pub fn known_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .packages
            .iter()
            .map(|package| package.name.clone())
            .chain(self.problems.iter().filter_map(|p| p.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

fn sorted_paths(dir: &Path, out: &mut Discovery) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            out.note(PluginError::Io {
                path: dir.to_path_buf(),
                source,
            });
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(source) => out.note(PluginError::Io {
                path: dir.to_path_buf(),
                source,
            }),
        }
    }
    paths.sort();
    paths
}

/// Discovers installed packages, or nothing when `--no-plugins` is set.
pub fn discover_installed(no_plugins: bool, bundled: &Bundled) -> Discovery {
    if no_plugins {
        return Discovery::default();
    }
    // An unresolvable data directory is not the same fact as an empty one.
    // Reported, because otherwise every installed package silently disappears.
    let site = match site_dir() {
        Ok(site) => site,
        Err(source) => {
            let mut out = Discovery::default();
            out.note(PluginError::PackageSiteUnavailable { source });
            return out;
        }
    };
    discover(&site, bundled)
}

/// Finds every manually installed package under `site`.
///
/// Returns them in a deterministic order, so two machines with the same
/// packages load them the same way. A missing site directory is not a problem;
/// it just means no packages are installed.
pub fn discover(site: &Path, bundled: &Bundled) -> Discovery {
    let mut out = Discovery::default();
    for group in sorted_paths(&site.join("pack"), &mut out) {
        if group.file_name().and_then(|n| n.to_str()) == Some(MANAGED_GROUP) {
            continue;
        }
        for (sub, eager) in [("start", true), ("opt", false)] {
            for dir in sorted_paths(&group.join(sub), &mut out) {
                let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.is_empty() || name.starts_with('.') {
                    continue;
                }
                let name = name.to_owned();
                let root = match dir.canonicalize() {
                    Ok(root) => root,
                    Err(source) => {
                        // A package that cannot even be resolved is reported
                        // rather than silently vanishing, since an unreadable
                        // directory looks identical to one that is not there.
                        out.refuse(name, PluginError::Io { path: dir, source });
                        continue;
                    }
                };
                if !root.is_dir() {
                    continue;
                }

                if bundled.is_bundled(&name) {
                    out.refuse(
                        name.clone(),
                        PluginError::PackageNameConflict { name, path: root },
                    );
                    continue;
                }
                let first = out
                    .packages
                    .iter()
                    .find(|p| p.name == name)
                    .map(|p| p.dir.clone());
                if let Some(first) = first {
                    out.refuse(
                        name.clone(),
                        PluginError::DuplicatePackage {
                            name,
                            first,
                            second: root,
                        },
                    );
                    continue;
                }

                let requested = match load_requested_permissions(&root) {
                    Ok(requested) => requested,
                    Err(problem) => {
                        out.refuse(name, problem);
                        continue;
                    }
                };
                out.packages.push(DiscoveredPackage {
                    name,
                    dir: root,
                    eager,
                    requested,
                    // Found by walking `pack/<group>/`, which is where a user
                    // puts files by hand. Maki's own checkouts are resolved
                    // through the lockfile, never discovered this way.
                    origin: Origin::Manual,
                    revision_guard: None,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use maki_pack::approvals::{ApprovalKey, Approvals, Entry};
    use maki_pack::lockfile::Owner;
    use test_case::test_case;
    use tokio::runtime::{EnterGuard, Handle};

    use crate::bundled::test_support::TEST_BUNDLED;
    use crate::error::PluginError;
    use crate::host::HOST_RUNTIME;
    use crate::plugin_permissions::{Component, PackageRequests, Permission, Requested};

    use super::{
        ACTIVE_DELETE_REFUSAL, APPROVALS_FILE, CONFLICTING_DECLARATIONS, DECLARED_DELETE_REFUSAL,
        DeleteTarget, DiscoveredPackage, FOREIGN_SOURCE_CONFLICT, InstallReport, Interaction,
        MANAGED_GROUP, OWNER_CONFLICT_FAILURE, Origin, PLUGIN_MANIFEST, PackCommand, PackContext,
        PackPreparation, PackRoots, PlannedOp, Problem, REVIEW_DELETE_LINE, REVIEW_PROMPT_HEADER,
        REVIEW_RELEASE_LINE, REVOKE_UI_APPROVAL_FAILURE, SHARED_UPDATE_REFUSAL,
        UI_APPROVAL_REQUIRED, UI_APPROVAL_STORE_UNREADABLE, UI_APPROVALS_FILE,
        UNAVAILABLE_OLD_REVISION, UNOWNED_DELETE_REFUSAL, UpdateOptions, UpdateTarget,
        apply_pack_plan, approval_prompt, approvals_path_in, discover, eligible, grant_installed,
        granted, install_declared_in, installed_from_declared_source, missing_grants,
        missing_requests, plan_owned, prepare_pack_ops_at, read_approvals_file, read_lockfile,
        resolved_on_disk, sanitize_message, update_permission_diff, write_approvals_at,
        write_lockfile,
    };

    const NET_MANIFEST: &str = "[permissions]\nnet = true\n";
    const UI_NET_MANIFEST: &str = "[permissions]\nfs_read = true\n\n[ui.permissions]\nnet = true\n";
    const APPROVAL_SRC: &str = "https://example.com/demo";
    const APPROVED_HOST: &str = "api.example.com";
    const NARROW_HOSTS_MANIFEST: &str =
        "[permissions]\nnet = true\nnet_hosts = [\"api.example.com\"]\n";
    const WIDENED_HOSTS_MANIFEST: &str =
        "[permissions]\nnet = true\nnet_hosts = [\"api.example.com\", \"api.other.example\"]\n";
    /// The extra host in [`WIDENED_HOSTS_MANIFEST`], as the approval prompt
    /// lists it.
    const MISSING_WIDER_HOST: &str = "net host api.other.example";
    /// What the approval prompt lists for a manifest that drops its hosts.
    const MISSING_ANY_HOST: &str = "net host *";
    const APPROVED_PINNED_HOST: &str = "api.example.com:7777";
    /// [`NARROW_HOSTS_MANIFEST`]'s host with no port, which reaches every port.
    const MISSING_UNPINNED_HOST: &str = "net host api.example.com";
    const APPROVED_TAKEOVER: &str = "greenpt";
    /// What the approval prompt lists for [`APPROVED_TAKEOVER`] when the
    /// approval never covered it.
    const MISSING_TAKEOVER: &str = "takes over provider 'greenpt'";
    const TEST_REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_REV: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn terminal_messages_keep_layout_but_remove_control_characters() {
        assert_eq!(
            sanitize_message("package\u{1b}[31m\rname\nnext"),
            "package [31m name\nnext"
        );
    }

    #[test]
    fn a_lockfile_read_error_is_not_treated_as_a_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();

        assert!(read_lockfile(Some(dir.path())).is_none());
    }

    #[test]
    fn a_missing_lockfile_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("missing.json");

        assert!(read_lockfile(Some(&path)).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_approval_store_is_not_treated_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("approvals.json");
        fs::write(&path, "not json").unwrap();

        assert!(read_approvals_file(&path).is_none());
    }

    #[test]
    fn a_missing_approval_store_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("approvals.json");

        assert!(read_approvals_file(&path).unwrap().is_empty());
    }

    fn make_package(site: &Path, group: &str, sub: &str, name: &str) -> PathBuf {
        let dir = site.join("pack").join(group).join(sub).join(name);
        fs::create_dir_all(dir.join("plugin")).unwrap();
        fs::write(dir.join("plugin").join("init.lua"), "").unwrap();
        dir
    }

    fn greedy_with(manifest: &str, name: &str, origin: Origin) -> DiscoveredPackage {
        let manifest = toml::from_str::<toml::Value>(manifest).unwrap();
        DiscoveredPackage {
            name: name.to_owned(),
            dir: PathBuf::from("/nowhere"),
            eager: true,
            requested: PackageRequests::from_manifest(&manifest),
            origin,
            revision_guard: None,
        }
    }

    /// Builds a package whose manifest asks for everything, which is the
    /// interesting case: the question is never what it asked for, but whose
    /// word turns the request into a grant.
    fn greedy(name: &str, origin: Origin) -> DiscoveredPackage {
        let asks: String = Permission::ALL
            .iter()
            .map(|perm| format!("{perm} = true\n"))
            .collect();
        greedy_with(
            &format!("providers = [\"greenpt\"]\n[permissions]\n{asks}"),
            name,
            origin,
        )
    }

    /// A package the user placed by hand is trusted like `init.lua`: they put
    /// the files there, so its manifest is their own statement.
    #[test]
    fn a_manual_package_is_granted_what_its_manifest_asks_for() {
        let pkg = greedy("demo", Origin::Manual);
        let (effective, takeovers) = granted(&pkg, &Approvals::default(), Component::Agent);

        for &perm in Permission::ALL {
            assert!(
                effective.is_allowed(perm),
                "{perm} should be granted to a manually installed package"
            );
        }
        assert_eq!(takeovers, ["greenpt".to_owned()]);
    }

    /// The negative half of the pair above, and the defect this exists to
    /// prevent: downloaded code must not be able to certify its own
    /// permissions by writing them into the manifest it ships.
    #[test]
    fn a_fetched_package_is_granted_nothing_without_an_approval() {
        let pkg = greedy(
            "demo",
            Origin::Fetched {
                src: "https://example.com/demo".to_owned(),
            },
        );
        let (effective, takeovers) = granted(&pkg, &Approvals::default(), Component::Agent);

        for &perm in Permission::ALL {
            assert!(
                !effective.is_allowed(perm),
                "{perm} must not be granted to fetched code with no approval"
            );
        }
        assert!(
            takeovers.is_empty(),
            "a takeover must not be granted to fetched code with no approval"
        );
    }

    /// An approval grants only what it names, and only where the package also
    /// asked for it. Both halves have to agree.
    #[test]
    fn a_fetched_package_is_granted_the_intersection_of_request_and_approval() {
        const GRANTED_TAKEOVER: &str = "greenpt";
        const UNGRANTED_TAKEOVER: &str = "rival";
        const NO_TAKEOVER_MESSAGE: &str = "a requested takeover the approval never named";
        let src = "https://example.com/demo";
        let manifest = format!(
            "providers = [{GRANTED_TAKEOVER:?}, {UNGRANTED_TAKEOVER:?}]\n[permissions]\nrun = true\nnet = true\n"
        );
        let pkg = greedy_with(
            manifest.as_str(),
            "demo",
            Origin::Fetched {
                src: src.to_owned(),
            },
        );

        let mut approvals = Approvals::default();
        approvals.approve(
            &ApprovalKey::new("demo", src),
            vec!["run".to_owned()],
            None,
            vec![GRANTED_TAKEOVER.to_owned()],
        );
        let (effective, takeovers) = granted(&pkg, &approvals, Component::Agent);

        assert!(effective.is_allowed(Permission::Run), "run was approved");
        assert!(
            !effective.is_allowed(Permission::Net),
            "net was requested but never approved"
        );
        assert_eq!(
            takeovers,
            [GRANTED_TAKEOVER.to_owned()],
            "{NO_TAKEOVER_MESSAGE}"
        );
    }

    /// The reason an approval is keyed by source as well as name. Pointing a
    /// name at another repository is a new trust decision, so the grants
    /// recorded for the old one must not carry over.
    #[test]
    fn an_approval_does_not_survive_a_changed_source() {
        let mut approvals = Approvals::default();
        approvals.approve(
            &ApprovalKey::new("demo", "https://example.com/demo"),
            vec!["run".to_owned()],
            None,
            Vec::new(),
        );

        let moved = greedy(
            "demo",
            Origin::Fetched {
                src: "https://elsewhere.example/demo".to_owned(),
            },
        );
        assert!(
            !granted(&moved, &approvals, Component::Agent)
                .0
                .is_allowed(Permission::Run),
            "an approval for one repository must not grant another"
        );
    }

    /// What a package with this manifest would still have to be asked about,
    /// given an approval for these permissions, these hosts and these
    /// takeovers, `None` being every host.
    fn still_missing(
        manifest: &str,
        permissions: &[&str],
        net_hosts: Option<&[&str]>,
        providers: &[&str],
    ) -> Vec<String> {
        let key = ApprovalKey::new("demo", APPROVAL_SRC);
        let mut store = Approvals::default();
        store.approve(
            &key,
            permissions.iter().map(|name| (*name).to_owned()).collect(),
            net_hosts.map(|hosts| hosts.iter().map(|host| (*host).to_owned()).collect()),
            providers.iter().map(|slug| (*slug).to_owned()).collect(),
        );
        let requested = Requested::from_manifest(
            &toml::from_str::<toml::Value>(manifest).unwrap(),
            Component::Agent,
        );
        missing_requests(&requested, store.get(&key))
    }

    #[test]
    fn every_unapproved_permission_is_reported() {
        assert_eq!(
            still_missing(
                "[permissions]\nfs_read = true\nnet = true\nrun = true\n",
                &["net"],
                None,
                &[]
            ),
            ["fs_read".to_owned(), "run".to_owned()]
        );
    }

    /// The defect `net_hosts` on an approval exists to prevent: an update that
    /// reaches further than the answer covers must ask again rather than
    /// inherit it, while an unchanged or narrower reach asks nothing. Dropping
    /// the list is the widest reach of all, not the absence of one.
    #[test_case(NARROW_HOSTS_MANIFEST, Some(&[APPROVED_HOST]), &[] ; "an_unchanged_host_list_needs_no_new_answer")]
    #[test_case(WIDENED_HOSTS_MANIFEST, Some(&[APPROVED_HOST]), &[MISSING_WIDER_HOST] ; "a_widened_host_list_asks_again")]
    #[test_case(NET_MANIFEST, Some(&[APPROVED_HOST]), &[MISSING_ANY_HOST] ; "a_dropped_host_list_asks_again")]
    #[test_case(NARROW_HOSTS_MANIFEST, None, &[] ; "a_list_narrows_an_approval_for_every_host")]
    #[test_case(NARROW_HOSTS_MANIFEST, Some(&[APPROVED_PINNED_HOST]), &[MISSING_UNPINNED_HOST] ; "unpinning_a_port_asks_again")]
    fn net_hosts_are_approved_like_permissions(
        manifest: &str,
        approved: Option<&[&str]>,
        expected: &[&str],
    ) {
        assert_eq!(still_missing(manifest, &["net"], approved, &[]), expected);
    }

    /// `providers` is approved the same way: a name the answer never covered
    /// asks again, because the package would be receiving a key the user
    /// never granted it for, while an approved takeover asks nothing.
    #[test_case(&[], &[MISSING_TAKEOVER] ; "an_unapproved_takeover_asks_again")]
    #[test_case(&[APPROVED_TAKEOVER], &[] ; "an_approved_takeover_asks_nothing")]
    fn taken_over_providers_are_approved_like_permissions(approved: &[&str], expected: &[&str]) {
        assert_eq!(
            still_missing(
                &format!("providers = [{APPROVED_TAKEOVER:?}]\n"),
                &[],
                None,
                approved
            ),
            expected
        );
    }

    /// The update prompt's takeover rows: a name the update adds is asking for
    /// a new key, and one it drops releases the old grant.
    #[test]
    fn an_update_lists_its_provider_takeovers() {
        const ADDED_TAKEOVER: &str = "rival";
        const ADDED_ROW: &str = "+takes over provider 'rival'";
        const DROPPED_ROW: &str = "-takes over provider 'greenpt'";
        let key = ApprovalKey::new("demo", APPROVAL_SRC);
        let mut store = Approvals::default();
        store.approve(&key, Vec::new(), None, vec![APPROVED_TAKEOVER.to_owned()]);
        let requested = Requested::from_manifest(
            &toml::from_str::<toml::Value>(&format!("providers = [{ADDED_TAKEOVER:?}]\n")).unwrap(),
            Component::Agent,
        );

        let diff = update_permission_diff(&requested, store.get(&key).unwrap());

        assert!(diff.contains(ADDED_ROW), "{diff}");
        assert!(diff.contains(DROPPED_ROW), "{diff}");
    }

    /// {spec} as both files declare it, eager and confirmed, so each
    /// component's approval is put to the user.
    fn declared_by_both(spec: maki_pack::Spec) -> crate::api::pack::Declared {
        crate::api::pack::Declared {
            spec,
            policies: Component::ALL
                .into_iter()
                .map(|component| {
                    let policy = crate::api::pack::Policy {
                        load: crate::api::pack::LoadMode::Eager,
                        confirm: true,
                        data: None,
                    };
                    (component, policy)
                })
                .collect(),
        }
    }

    fn declared_pack(name: &str, src: &str) -> crate::api::pack::Declared {
        declared_by_both(maki_pack::Spec::new(src).with_name(name))
    }

    /// Sets up a site where `name` is installed from `src` at one revision.
    fn installed_site(name: &str, src: &str) -> (tempfile::TempDir, maki_pack::lockfile::Lockfile) {
        let site = tempfile::TempDir::new().unwrap();
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record(name, src, TEST_REV);
        fs::create_dir_all(maki_pack::paths::revision_dir(site.path(), name, TEST_REV)).unwrap();
        (site, lock)
    }

    /// The rule that decides whether installing is a fresh trust decision.
    /// `.maki/init.lua` is project local, so a repository maki opens can point
    /// a name the user already trusts somewhere else: matching on the name
    /// alone skipped the prompt and cloned the new source on the strength of
    /// the old decision.
    #[test]
    fn a_package_is_only_installed_when_the_recorded_source_matches() {
        let src = "https://example.com/demo";
        let (site, lock) = installed_site("demo", src);
        let manager = maki_pack::manager::Manager::new(site.path());

        assert!(
            installed_from_declared_source(&declared_pack("demo", src), &lock, &manager),
            "same name, same source, and on disk"
        );
        assert!(
            !installed_from_declared_source(
                &declared_pack("demo", "https://elsewhere.example/demo"),
                &lock,
                &manager
            ),
            "a name pointed at another repository is a new trust decision"
        );
    }

    /// A held lock stops an install, but it does not make the packages already
    /// on disk disappear. Discovery skips the managed group, so reporting none
    /// left the session with every managed package missing.
    #[test]
    fn packages_already_on_disk_resolve_without_git_or_a_lock() {
        let src = "https://example.com/demo";
        let (site, lock) = installed_site("demo", src);

        let mut failures = Vec::new();
        let found = resolved_on_disk(
            &[&declared_pack("demo", src)],
            site.path(),
            &lock,
            &Component::ALL,
            &mut failures,
        );
        assert_eq!(found.len(), 1, "the installed package is still usable");
        assert_eq!(found[0].name, "demo");
        assert_eq!(
            found[0].dir,
            maki_pack::paths::revision_dir(site.path(), "demo", TEST_REV)
        );

        let moved = resolved_on_disk(
            &[&declared_pack("demo", "https://elsewhere.example/demo")],
            site.path(),
            &lock,
            &Component::ALL,
            &mut failures,
        );
        assert!(
            moved.is_empty(),
            "the recorded revision describes the old source only"
        );
    }

    /// A package maki itself refused is still a name the user can write in
    /// `plugins.<name>`. Validating a config against the loadable set alone
    /// turned one bad manifest into a startup failure that blamed the config.
    #[test]
    fn a_package_that_cannot_be_read_is_still_a_known_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", "start", "broken");
        fs::write(dir.join("plugin.toml"), "not = valid = toml").unwrap();

        let found = discover(tmp.path(), &TEST_BUNDLED);
        assert!(found.packages.is_empty(), "the package must not load");
        assert_eq!(found.problems.len(), 1, "and the reason must be reported");
        assert_eq!(found.known_names(), vec!["broken".to_owned()]);
    }

    #[test]
    fn missing_site_dir_is_not_a_problem() {
        let tmp = tempfile::TempDir::new().unwrap();
        let found = discover(&tmp.path().join("absent"), &TEST_BUNDLED);
        assert!(found.packages.is_empty());
        assert!(found.problems.is_empty());
    }

    #[test]
    fn unreadable_package_root_is_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pack = tmp.path().join("pack");
        fs::write(&pack, "not a directory").unwrap();

        let found = discover(tmp.path(), &TEST_BUNDLED);

        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::Io { .. },
                ..
            }]
        ));
    }

    #[test]
    fn finds_start_and_opt_and_marks_eagerness() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "eager_one");
        make_package(tmp.path(), "vendor", "opt", "lazy_one");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        assert_eq!(found.packages.len(), 2);

        let eager = found
            .packages
            .iter()
            .find(|p| p.name == "eager_one")
            .unwrap();
        let lazy = found
            .packages
            .iter()
            .find(|p| p.name == "lazy_one")
            .unwrap();
        assert!(eager.eager, "start/ packages load at startup");
        assert!(!lazy.eager, "opt/ packages wait to be activated");
    }

    /// Managed packages are resolved from recorded state, so finding them here
    /// too would give one package two identities.
    #[test]
    fn managed_group_is_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), MANAGED_GROUP, "opt", "managed_one");
        make_package(tmp.path(), "vendor", "start", "manual_one");

        let names: Vec<String> = discover(tmp.path(), &TEST_BUNDLED)
            .packages
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["manual_one"]);
    }

    #[test]
    fn bundled_name_collision_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "bash");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::PackageNameConflict { .. },
                ..
            }]
        ));
    }

    /// `lib` is bundled without being enabled by default, so a name check
    /// against the default set alone would let it through.
    #[test]
    fn non_default_bundled_name_is_also_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "lib");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        assert!(found.packages.is_empty());
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::PackageNameConflict { .. },
                ..
            }]
        ));
    }

    #[test]
    fn duplicate_names_across_groups_are_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "alpha", "start", "twice");
        make_package(tmp.path(), "beta", "start", "twice");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        assert_eq!(found.packages.len(), 1, "the first one still loads");
        assert!(matches!(
            found.problems.as_slice(),
            [Problem {
                error: PluginError::DuplicatePackage { .. },
                ..
            }]
        ));
    }

    /// One unusable package must not take the others down with it.
    #[test]
    fn a_refused_package_does_not_stop_the_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "bash");
        make_package(tmp.path(), "vendor", "start", "fine_one");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        let names: Vec<&str> = found.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["fine_one"]);
        assert_eq!(found.problems.len(), 1);
    }

    #[test]
    fn manifest_permissions_are_read_and_deny_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", "start", "asks");
        fs::write(dir.join("plugin.toml"), NET_MANIFEST).unwrap();

        let found = discover(tmp.path(), &TEST_BUNDLED);
        let pkg = &found.packages[0];
        assert!(pkg.requested.agent.is_requested(Permission::Net));
        assert!(!pkg.requested.agent.is_requested(Permission::Run));
    }

    #[test]
    fn a_ui_table_is_the_ui_components_request_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", "start", "both");
        fs::write(dir.join("plugin.toml"), UI_NET_MANIFEST).unwrap();

        let found = discover(tmp.path(), &TEST_BUNDLED);
        let requested = &found.packages[0].requested;
        assert!(requested.ui.is_requested(Permission::Net));
        assert!(!requested.agent.is_requested(Permission::Net));
        assert!(requested.agent.is_requested(Permission::FsRead));
        assert!(!requested.ui.is_requested(Permission::FsRead));
    }

    #[test]
    fn package_without_manifest_requests_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_package(tmp.path(), "vendor", "start", "silent");

        let found = discover(tmp.path(), &TEST_BUNDLED);
        for perm in [
            Permission::FsRead,
            Permission::FsWrite,
            Permission::Net,
            Permission::Run,
            Permission::Env,
        ] {
            assert!(!found.packages[0].requested.agent.is_requested(perm));
        }
    }

    fn declaration(name: &str) -> crate::api::pack::Declared {
        declared_by_both(
            maki_pack::Spec::new(format!("https://example.com/{name}")).with_name(name),
        )
    }

    /// Holds the runtime the sync pack functions block on, as `main` does.
    struct UpdateFixture {
        _runtime: EnterGuard<'static>,
        _temp: tempfile::TempDir,
        site: PathBuf,
        lock_path: PathBuf,
        declared: Vec<crate::api::pack::Declared>,
        old_rev: String,
        new_rev: String,
    }

    fn run_git(repo: &Path, args: &[&str]) -> maki_pack::git::GitOutput {
        Handle::current()
            .block_on(maki_pack::git::run(
                args.iter().map(|arg| (*arg).to_owned()).collect(),
                repo.to_path_buf(),
            ))
            .unwrap()
    }

    fn update_fixture() -> UpdateFixture {
        update_fixture_with_manifests(None, None)
    }

    fn update_fixture_with_manifests(
        old_manifest: Option<&str>,
        new_manifest: Option<&str>,
    ) -> UpdateFixture {
        let runtime = HOST_RUNTIME.enter();
        let temp = tempfile::TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        fs::create_dir_all(origin.join("plugin")).unwrap();
        fs::write(origin.join("plugin").join("init.lua"), "-- first\n").unwrap();
        if let Some(manifest) = old_manifest {
            fs::write(origin.join(PLUGIN_MANIFEST), manifest).unwrap();
        }
        run_git(&origin, &["init", "--quiet"]);
        run_git(&origin, &["config", "user.email", "test@example.com"]);
        run_git(&origin, &["config", "user.name", "test"]);
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "first"]);

        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        let spec = maki_pack::Spec::new(origin.display().to_string()).with_name("demo");
        let mut lock = maki_pack::lockfile::Lockfile::default();
        let installed = Handle::current()
            .block_on(maki_pack::manager::Manager::new(&site).ensure_installed(&spec, &mut lock))
            .unwrap();
        assert!(write_lockfile(&lock_path, &lock));

        fs::write(origin.join("plugin").join("later.lua"), "-- later\n").unwrap();
        match new_manifest {
            Some(manifest) => fs::write(origin.join(PLUGIN_MANIFEST), manifest).unwrap(),
            None if old_manifest.is_some() => {
                fs::remove_file(origin.join(PLUGIN_MANIFEST)).unwrap()
            }
            None => {}
        }
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "later"]);
        let new_rev = run_git(&origin, &["rev-parse", "HEAD"])
            .stdout
            .trim()
            .to_owned();
        let declared = vec![declared_by_both(spec)];

        UpdateFixture {
            _runtime: runtime,
            _temp: temp,
            site,
            lock_path,
            declared,
            old_rev: installed.rev,
            new_rev,
        }
    }

    struct SiteFixture {
        _runtime: EnterGuard<'static>,
        _temp: tempfile::TempDir,
        site: PathBuf,
        lock_path: PathBuf,
    }

    /// A site holding the given `(name, revision)` packages, recorded in the
    /// lockfile and present on disk, with nothing behind them in git.
    fn managed_site(packages: &[(&str, &str)]) -> SiteFixture {
        let temp = tempfile::TempDir::new().unwrap();
        let site = temp.path().join("site");
        let lock_path = temp.path().join("pack-lock.json");
        let mut lock = maki_pack::lockfile::Lockfile::default();
        for (name, revision) in packages {
            lock.record(*name, format!("https://example.com/{name}"), *revision);
            fs::create_dir_all(maki_pack::paths::revision_dir(&site, name, revision)).unwrap();
        }
        assert!(write_lockfile(&lock_path, &lock));
        SiteFixture {
            _runtime: HOST_RUNTIME.enter(),
            _temp: temp,
            site,
            lock_path,
        }
    }

    /// Names {owners} as the owners of `demo` in the lockfile at {lock_path}.
    fn set_owners(lock_path: &Path, owners: &[Owner]) {
        let mut lock = read_lockfile(Some(lock_path)).unwrap();
        lock.set_owners(DEMO, owners);
        assert!(write_lockfile(lock_path, &lock));
    }

    /// The owners of `demo` in the lockfile at {lock_path}, `None` without
    /// an entry.
    fn lock_owners(lock_path: &Path) -> Option<Vec<Owner>> {
        read_lockfile(Some(lock_path))
            .unwrap()
            .get(DEMO)
            .map(|entry| entry.owners.clone())
    }

    fn delete_operation(name: &str, force: bool) -> PlannedOp {
        PlannedOp::Delete {
            name: name.to_owned(),
            force,
        }
    }

    fn update_operation(force: bool) -> PlannedOp {
        PlannedOp::Update {
            name: "demo".to_owned(),
            options: UpdateOptions {
                force,
                target: UpdateTarget::Version,
            },
        }
    }

    fn finish_preparation(preparation: PackPreparation) -> (super::PackReport, Option<String>) {
        match preparation {
            PackPreparation::Complete(report) => (report, None),
            PackPreparation::Ready(plan) => (apply_pack_plan(plan), None),
            PackPreparation::Review { prompt, plan } => (apply_pack_plan(plan), Some(prompt)),
        }
    }

    fn approve(fixture: &UpdateFixture, names: &[&str]) {
        approve_reaching(fixture, names, None);
    }

    fn approve_reaching(fixture: &UpdateFixture, names: &[&str], hosts: Option<&[&str]>) {
        let path = fixture.site.join(APPROVALS_FILE);
        let mut approvals = Approvals::default();
        approvals.approve(
            &ApprovalKey::new("demo", &fixture.declared[0].spec.src),
            names.iter().map(|name| (*name).to_owned()).collect(),
            hosts.map(|hosts| hosts.iter().map(|host| (*host).to_owned()).collect()),
            Vec::new(),
        );
        assert!(write_approvals_at(&path, &approvals));
    }

    /// Dropping `net_hosts` lifts every limit, so the review must say the
    /// package now reaches every host rather than read like it narrowed.
    #[test_case(NARROW_HOSTS_MANIFEST, NET_MANIFEST, "+net host *" ; "a_dropped_list_widens_to_every_host")]
    #[test_case(NET_MANIFEST, NARROW_HOSTS_MANIFEST, "-net host *" ; "a_new_list_narrows_from_every_host")]
    fn update_review_shows_the_reach_that_changed(
        old_manifest: &str,
        new_manifest: &str,
        expected: &str,
    ) {
        let fixture = update_fixture_with_manifests(Some(old_manifest), Some(new_manifest));
        let approved_hosts: &[&str] = &[APPROVED_HOST];
        let reach = (old_manifest == NARROW_HOSTS_MANIFEST).then_some(approved_hosts);
        approve_reaching(&fixture, &["net"], reach);
        let (_, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        let prompt = prompt.expect("a normal update needs review");
        assert!(
            prompt.contains(&format!("permissions: {expected}")),
            "{prompt}"
        );
    }

    #[test]
    fn accepted_update_review_applies_the_prepared_revision() {
        let fixture = update_fixture();
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            report.updated,
            vec![("demo".to_owned(), fixture.new_rev.clone())]
        );
        assert!(prompt.contains(&fixture.old_rev));
        assert!(prompt.contains(&fixture.new_rev));
        assert!(prompt.contains("permissions: unchanged"));
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.new_rev
        );
    }

    #[test_case(
        &[],
        None,
        Some(NET_MANIFEST),
        "+net";
        "added_permission"
    )]
    #[test_case(
        &["fs_write"],
        Some("[permissions]\nfs_write = true\n"),
        Some(NET_MANIFEST),
        "+net, -fs_write";
        "added_and_removed_permissions"
    )]
    #[test_case(
        &["run"],
        Some("[permissions]\nrun = true\n"),
        None,
        "-run";
        "removed_permission"
    )]
    #[test_case(
        &["net", "run"],
        Some(NET_MANIFEST),
        Some("[permissions]\nnet = true\nrun = true\n"),
        "unchanged";
        "new_request_was_already_approved"
    )]
    #[test_case(
        &["net"],
        Some("[permissions]\nnet = true\nrun = true\n"),
        Some("[permissions]\nnet = true\nrun = true\n"),
        "+run";
        "unchanged_manifest_still_needs_missing_approval"
    )]
    fn update_review_shows_permission_changes(
        approved: &[&str],
        old_manifest: Option<&str>,
        new_manifest: Option<&str>,
        expected: &str,
    ) {
        let fixture = update_fixture_with_manifests(old_manifest, new_manifest);
        approve(&fixture, approved);
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains(&format!("permissions: {expected}")));
    }

    #[test]
    fn invalid_proposed_manifest_prevents_forced_apply() {
        let fixture = update_fixture_with_manifests(None, Some("permissions = ["));

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(true)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(prompt.is_none());
        assert!(report.updated.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("is not valid toml"));
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.old_rev
        );
        assert!(!maki_pack::paths::revision_dir(&fixture.site, "demo", &fixture.new_rev).exists());
    }

    #[test]
    fn invalid_previous_manifest_is_display_only() {
        let fixture = update_fixture_with_manifests(Some("permissions = ["), Some(NET_MANIFEST));
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains("permissions: +net"));
    }

    #[test]
    fn unavailable_previous_manifest_is_reported_in_review() {
        let fixture =
            update_fixture_with_manifests(Some("[permissions]\nrun = true\n"), Some(NET_MANIFEST));
        let mut lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        lock.record("demo", &fixture.declared[0].spec.src, TEST_REV);
        assert!(write_lockfile(&fixture.lock_path, &lock));
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains(UNAVAILABLE_OLD_REVISION.trim()));
        assert!(prompt.contains("permissions: +net"));
    }

    #[test]
    fn declined_update_review_changes_no_managed_state() {
        let fixture = update_fixture();
        let preparation = prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        );
        let PackPreparation::Review { plan, .. } = preparation else {
            panic!("a normal update needs review");
        };
        drop(plan);
        assert!(!maki_pack::paths::revision_dir(&fixture.site, "demo", &fixture.new_rev).exists());
        assert_eq!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .unwrap()
                .rev,
            fixture.old_rev
        );
    }

    #[test]
    fn forced_update_skips_review_and_applies_the_revision() {
        let fixture = update_fixture();
        let approvals = fixture.site.join(APPROVALS_FILE);
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(true)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(prompt.is_none());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.updated, vec![("demo".to_owned(), fixture.new_rev)]);
        assert!(
            !approvals.exists(),
            "force must not write permission approval"
        );
    }

    #[test_case(false, &["net"]; "accepted_review_grants_what_it_showed")]
    #[test_case(true, &[]; "force_grants_nothing")]
    fn applied_update_records_only_reviewed_permissions(force: bool, expected: &[&str]) {
        let fixture = update_fixture_with_manifests(None, Some(NET_MANIFEST));
        let (report, _) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(force)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let approvals = read_approvals_file(&fixture.site.join(APPROVALS_FILE))
            .expect("the approval store stays readable");
        let key = ApprovalKey::new("demo", &fixture.declared[0].spec.src);
        assert_eq!(
            approvals
                .get(&key)
                .map(Entry::permissions)
                .unwrap_or_default(),
            expected
        );
    }

    #[test]
    fn unreadable_approval_store_stops_before_git() {
        let fixture = update_fixture();
        let approval_path = fixture.site.join(APPROVALS_FILE);
        fs::create_dir_all(&fixture.site).unwrap();
        fs::write(&approval_path, "not json").unwrap();
        let work = maki_pack::paths::package_root(&fixture.site, "demo").join(".work");
        let before = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(prompt.is_none());
        assert!(report.updated.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("approval store is unreadable"));
        let after = run_git(&work, &["rev-parse", "refs/remotes/origin/HEAD"])
            .stdout
            .trim()
            .to_owned();
        assert_eq!(before, after, "the work clone must not fetch");
    }

    #[test]
    fn delete_batch_removes_inactive_packages_and_keeps_active_ones() {
        let fixture = managed_site(&[("inactive", TEST_REV), ("active", OTHER_REV)]);
        let context = PackContext::new(Vec::new(), vec!["inactive".into()], Default::default())
            .merge(PackContext::new(
                Vec::new(),
                vec!["active".into()],
                BTreeSet::from(["active".into()]),
            ))
            .unwrap();
        let operations = super::plan_owned(
            &PackCommand::Delete {
                target: DeleteTarget::Undeclared,
                force: false,
            },
            &context,
            &Component::ALL,
            &fixture.lock_path,
        )
        .unwrap();

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &operations,
            &context.declared,
            &context.active,
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert_eq!(
            prompt,
            Some(format!(
                "{REVIEW_PROMPT_HEADER}\n  inactive: {REVIEW_DELETE_LINE}"
            ))
        );
        assert_eq!(report.removed, vec!["inactive"]);
        assert_eq!(
            report.failures,
            vec![format!("active: {ACTIVE_DELETE_REFUSAL}")]
        );
        assert!(!maki_pack::paths::package_root(&fixture.site, "inactive").exists());
        assert!(maki_pack::paths::package_root(&fixture.site, "active").is_dir());
        let lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        assert!(lock.get("inactive").is_none());
        assert!(lock.get("active").is_some());
    }

    /// The whole point of the review: a declined plan is a plan that never
    /// touched the disk.
    #[test]
    fn deletion_is_reviewed_before_anything_is_removed() {
        let fixture = managed_site(&[("orphan", TEST_REV)]);

        let preparation = prepare_pack_ops_at(
            &[delete_operation("orphan", false)],
            &[],
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        );

        let PackPreparation::Review { prompt, .. } = preparation else {
            panic!("a delete needs review");
        };
        assert_eq!(
            prompt,
            format!("{REVIEW_PROMPT_HEADER}\n  orphan: {REVIEW_DELETE_LINE}")
        );
        assert!(maki_pack::paths::package_root(&fixture.site, "orphan").is_dir());
    }

    #[test]
    fn forced_deletion_skips_review_and_removes_the_package() {
        let fixture = managed_site(&[("orphan", TEST_REV)]);

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation("orphan", true)],
            &[],
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(prompt.is_none());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed, vec!["orphan"]);
        assert!(!maki_pack::paths::package_root(&fixture.site, "orphan").exists());
    }

    #[test]
    fn updates_refuse_bundled_and_manual_package_owners() {
        let fixture = managed_site(&[]);
        make_package(&fixture.site, "vendor", "start", "manual");

        for name in ["bash", "manual"] {
            let (report, prompt) = finish_preparation(prepare_pack_ops_at(
                &[PlannedOp::Update {
                    name: name.to_owned(),
                    options: UpdateOptions::default(),
                }],
                &[declaration(name)],
                &Default::default(),
                &Component::ALL,
                &fixture.site,
                &fixture.lock_path,
                &TEST_BUNDLED,
            ));

            assert!(prompt.is_none());
            assert_eq!(
                report.failures,
                vec![format!("{name}: {OWNER_CONFLICT_FAILURE}")]
            );
        }
    }

    #[test]
    fn deletion_refuses_a_declared_package_before_removing_files() {
        let fixture = managed_site(&[("demo", TEST_REV)]);

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation("demo", false)],
            &[declaration("demo")],
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(prompt.is_none());
        assert_eq!(
            report.failures,
            vec![format!("demo: {DECLARED_DELETE_REFUSAL}")]
        );
        assert!(maki_pack::paths::package_root(&fixture.site, "demo").is_dir());
        assert!(
            read_lockfile(Some(&fixture.lock_path))
                .unwrap()
                .get("demo")
                .is_some()
        );
    }

    #[test]
    fn a_bang_forces_either_command_and_removed_flags_are_rejected() {
        assert_eq!(
            super::PackCommand::parse("/packupdate", "++lockfile demo", true).unwrap(),
            super::PackCommand::Update {
                name: Some("demo".to_owned()),
                options: UpdateOptions {
                    force: true,
                    target: UpdateTarget::Lockfile,
                },
            }
        );
        assert_eq!(
            super::PackCommand::parse("/packdel", "++all", true).unwrap(),
            super::PackCommand::Delete {
                target: super::DeleteTarget::Undeclared,
                force: true,
            }
        );
        assert!(super::PackCommand::parse("/packupdate", "++offline", false).is_err());
        assert!(super::PackCommand::parse("/packdel", "++lockfile demo", false).is_err());
    }

    #[test_case("/packupdate", "one two" ; "two_packages_to_update")]
    #[test_case("/packupdate", "++all" ; "update_everything")]
    #[test_case("/packdel", "one two" ; "two_packages_to_delete")]
    #[test_case("/packdel", "" ; "nothing_to_delete")]
    #[test_case("/packdel", "++all demo" ; "all_and_a_name")]
    #[test_case("/packls", "" ; "not_a_package_command")]
    fn parsing_rejects_what_planning_would_have_no_answer_for(name: &str, args: &str) {
        assert!(super::PackCommand::parse(name, args, false).is_err());
    }

    #[test]
    fn command_plan_keeps_update_and_delete_boundaries() {
        let declared = vec![declaration("alpha"), declaration("missing")];
        let installed = vec!["alpha".to_owned(), "orphan".to_owned()];
        let update = super::PackCommand::parse("/packupdate", "", false).unwrap();
        assert_eq!(
            super::plan_command(&update, &declared, &installed).unwrap(),
            vec![PlannedOp::Update {
                name: "alpha".to_owned(),
                options: UpdateOptions::default(),
            }]
        );

        let undeclared_update = super::PackCommand::parse("/packupdate", "orphan", false).unwrap();
        assert!(super::plan_command(&undeclared_update, &declared, &installed).is_err());
        let uninstalled_update =
            super::PackCommand::parse("/packupdate", "missing", false).unwrap();
        assert!(super::plan_command(&uninstalled_update, &declared, &installed).is_err());

        let declared_delete = super::PackCommand::parse("/packdel", "alpha", false).unwrap();
        assert!(super::plan_command(&declared_delete, &declared, &installed).is_err());
        let missing_delete = super::PackCommand::parse("/packdel", "absent", false).unwrap();
        assert!(super::plan_command(&missing_delete, &declared, &installed).is_err());
        let delete_all = super::PackCommand::parse("/packdel", "++all", false).unwrap();
        assert_eq!(
            super::plan_command(&delete_all, &declared, &installed).unwrap(),
            vec![delete_operation("orphan", false)]
        );
    }

    const DEMO: &str = "demo";
    const MOVED_SRC: &str = "https://elsewhere.example/demo";
    const UI_ONLY_MANIFEST: &str = "[ui.permissions]\nnet = true\n";
    const EMPTY_UI_MANIFEST: &str = "[ui]\n";
    const NET: &str = "net";
    const FS_READ: &str = "fs_read";
    const UI_NEW_NET_LINE: &str = "ui permissions (new): net";
    const UI_DROPPED_NET_LINE: &str = "ui permissions: -net";
    const UI_REVIEW_LINE: &str = "ui permissions";

    fn fetched(manifest: &str, src: &str) -> DiscoveredPackage {
        greedy_with(
            manifest,
            DEMO,
            Origin::Fetched {
                src: src.to_owned(),
            },
        )
    }

    fn store_with(src: &str, permissions: &[&str]) -> Approvals {
        let mut store = Approvals::default();
        store.approve(
            &ApprovalKey::new(DEMO, src),
            permissions.iter().map(|name| (*name).to_owned()).collect(),
            None,
            Vec::new(),
        );
        store
    }

    fn write_store(site: &Path, component: Component, src: &str, permissions: &[&str]) {
        assert!(write_approvals_at(
            &approvals_path_in(site, component),
            &store_with(src, permissions)
        ));
    }

    fn read_store(site: &Path, component: Component) -> Approvals {
        read_approvals_file(&approvals_path_in(site, component)).expect("a readable store")
    }

    fn approved_in(site: &Path, component: Component, src: &str) -> Option<Vec<String>> {
        read_store(site, component)
            .get(&ApprovalKey::new(DEMO, src))
            .map(|entry| entry.permissions().to_vec())
    }

    /// A site where `demo` is installed from {src} with {manifest}.
    fn installed_with_manifest(
        src: &str,
        manifest: &str,
    ) -> (tempfile::TempDir, maki_pack::lockfile::Lockfile) {
        let (site, lock) = installed_site(DEMO, src);
        let dir = maki_pack::paths::revision_dir(site.path(), DEMO, TEST_REV);
        fs::write(dir.join(PLUGIN_MANIFEST), manifest).unwrap();
        (site, lock)
    }

    /// One install pass over `demo` declared from {src}, answering every
    /// question with {answer}, and the questions it asked.
    fn grant_pass(
        site: &Path,
        lock: &maki_pack::lockfile::Lockfile,
        src: &str,
        components: &[Component],
        answer: bool,
    ) -> (InstallReport, Vec<String>) {
        let manager = maki_pack::manager::Manager::new(site);
        let declared = declared_pack(DEMO, src);
        let mut prompts = Vec::new();
        let mut report = InstallReport::default();
        grant_installed(
            &[&declared],
            site,
            &manager,
            lock,
            components,
            &mut |prompt| {
                prompts.push(prompt.to_owned());
                answer
            },
            &mut report,
        );
        (report, prompts)
    }

    /// Every approval written before components existed sits in the agent
    /// store, which keeps its file and its meaning; the UI has none.
    #[test]
    fn a_store_from_before_components_is_the_agent_store_unchanged() {
        const LEGACY_STORE: &str =
            r#"{"entries":{"demo":{"src":"https://example.com/demo","permissions":["net"]}}}"#;
        let site = tempfile::TempDir::new().unwrap();
        fs::write(site.path().join(APPROVALS_FILE), LEGACY_STORE).unwrap();
        let pkg = fetched(NET_MANIFEST, APPROVAL_SRC);

        let agent = read_store(site.path(), Component::Agent);
        let ui = read_store(site.path(), Component::Ui);

        assert!(eligible(&pkg, &agent, Component::Agent));
        assert!(
            granted(&pkg, &agent, Component::Agent)
                .0
                .is_allowed(Permission::Net)
        );
        assert!(
            ui.is_empty(),
            "no UI approval exists before the UI file does"
        );
        assert_eq!(
            fs::read_to_string(site.path().join(APPROVALS_FILE)).unwrap(),
            LEGACY_STORE
        );
    }

    /// The UI entry is new code: an agent approval, however wide, never
    /// vouches for it, while a UI record does, even an empty one.
    #[test_case(UI_ONLY_MANIFEST, None, false ; "an_agent_approval_alone")]
    #[test_case(EMPTY_UI_MANIFEST, None, false ; "an_agent_approval_alone_for_an_empty_ui")]
    #[test_case(EMPTY_UI_MANIFEST, Some(&[]), true ; "an_empty_ui_record")]
    #[test_case(UI_ONLY_MANIFEST, Some(&[]), false ; "a_ui_record_missing_a_request")]
    #[test_case(UI_ONLY_MANIFEST, Some(&[NET]), true ; "a_ui_record_covering_the_request")]
    fn a_fetched_ui_component_needs_a_ui_record(
        manifest: &str,
        ui_record: Option<&[&str]>,
        expected: bool,
    ) {
        let site = tempfile::TempDir::new().unwrap();
        let pkg = fetched(manifest, APPROVAL_SRC);
        write_store(site.path(), Component::Agent, APPROVAL_SRC, &[NET, FS_READ]);
        if let Some(names) = ui_record {
            write_store(site.path(), Component::Ui, APPROVAL_SRC, names);
        }

        let agent = read_store(site.path(), Component::Agent);
        let ui = read_store(site.path(), Component::Ui);

        assert!(eligible(&pkg, &agent, Component::Agent));
        assert_eq!(eligible(&pkg, &ui, Component::Ui), expected);
    }

    /// A hand-installed package grants each component its own table, as
    /// the agent's always was, in `start` and `opt` alike.
    #[test_case("start" ; "start")]
    #[test_case("opt" ; "opt")]
    fn a_manual_package_grants_each_component_its_own_table(sub: &str) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = make_package(tmp.path(), "vendor", sub, DEMO);
        fs::write(dir.join(PLUGIN_MANIFEST), UI_NET_MANIFEST).unwrap();
        let found = discover(tmp.path(), &TEST_BUNDLED);
        let pkg = &found.packages[0];
        let none = Approvals::default();

        let (agent, _) = granted(pkg, &none, Component::Agent);
        let (ui, providers) = granted(pkg, &none, Component::Ui);

        assert!(agent.is_allowed(Permission::FsRead) && !agent.is_allowed(Permission::Net));
        assert!(ui.is_allowed(Permission::Net) && !ui.is_allowed(Permission::FsRead));
        assert!(providers.is_empty());
        for component in Component::ALL {
            assert!(eligible(pkg, &none, component), "{component:?}");
        }
    }

    /// The install pass asks once per component that needs it, the agent
    /// first, and only for the components the host loads; a package with no
    /// `[ui]` table asks what it always asked.
    #[test_case(UI_NET_MANIFEST, &Component::ALL, 2 ; "both_components")]
    #[test_case(UI_NET_MANIFEST, &[Component::Agent], 1 ; "an_agent_host")]
    #[test_case(UI_NET_MANIFEST, &[Component::Ui], 1 ; "a_ui_host")]
    #[test_case(NET_MANIFEST, &Component::ALL, 1 ; "no_ui_table")]
    fn the_install_pass_asks_per_component(
        manifest: &str,
        components: &[Component],
        questions: usize,
    ) {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, manifest);

        let (report, prompts) = grant_pass(site.path(), &lock, APPROVAL_SRC, components, true);

        assert_eq!(prompts.len(), questions, "{prompts:?}");
        assert_eq!(report.packages.len(), 1, "{:?}", report.failures);
        assert_eq!(
            site.path().join(UI_APPROVALS_FILE).exists(),
            components.contains(&Component::Ui) && manifest == UI_NET_MANIFEST
        );
    }

    /// Only the components a file declares are asked about, and the first
    /// of them leads: a package `ui.lua` alone declares stands on its UI
    /// approval, with no question about its agent.
    #[test_case(Component::Agent ; "declared_in_init_lua")]
    #[test_case(Component::Ui ; "declared_in_ui_lua")]
    fn the_install_pass_asks_only_about_declared_components(component: Component) {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, UI_NET_MANIFEST);
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut declared = declared_pack(DEMO, APPROVAL_SRC);
        declared
            .policies
            .retain(|&declares, _| declares == component);
        let mut prompts = Vec::new();
        let mut report = InstallReport::default();

        grant_installed(
            &[&declared],
            site.path(),
            &manager,
            &lock,
            &Component::ALL,
            &mut |prompt| {
                prompts.push(prompt.to_owned());
                false
            },
            &mut report,
        );

        let pkg = fetched(UI_NET_MANIFEST, APPROVAL_SRC);
        let missing = missing_grants(&pkg, &Approvals::default(), component);
        assert_eq!(prompts, [approval_prompt(DEMO, component, &missing)]);
        assert!(report.packages.is_empty(), "the leading refusal skips it");
    }

    /// Approving the UI component writes the UI file alone: the agent's
    /// approval is neither rewritten nor moved.
    #[test]
    fn approving_the_ui_writes_only_the_ui_file() {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, UI_NET_MANIFEST);
        write_store(site.path(), Component::Agent, APPROVAL_SRC, &[FS_READ]);
        let agent_before = fs::read(site.path().join(APPROVALS_FILE)).unwrap();

        let (report, prompts) = grant_pass(site.path(), &lock, APPROVAL_SRC, &Component::ALL, true);

        assert_eq!(prompts.len(), 1, "only the UI was unapproved: {prompts:?}");
        assert_eq!(report.packages.len(), 1);
        assert_eq!(
            fs::read(site.path().join(APPROVALS_FILE)).unwrap(),
            agent_before
        );
        assert_eq!(
            approved_in(site.path(), Component::Ui, APPROVAL_SRC),
            Some(vec![NET.to_owned()])
        );
    }

    /// A refused UI keeps the package, whose agent passed its own gate, and
    /// leaves the UI unapproved; a refused agent skips the package before
    /// its UI is asked about.
    #[test_case(&[FS_READ], 1, 1 ; "a_refused_ui")]
    #[test_case(&[], 1, 0 ; "a_refused_agent")]
    fn a_refusal_holds_back_only_its_component(
        agent_approved: &[&str],
        questions: usize,
        packages: usize,
    ) {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, UI_NET_MANIFEST);
        write_store(site.path(), Component::Agent, APPROVAL_SRC, agent_approved);

        let (report, prompts) =
            grant_pass(site.path(), &lock, APPROVAL_SRC, &Component::ALL, false);

        assert_eq!(prompts.len(), questions, "{prompts:?}");
        assert_eq!(report.packages.len(), packages);
        assert_eq!(approved_in(site.path(), Component::Ui, APPROVAL_SRC), None);
        if packages == 1 {
            assert!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains(UI_APPROVAL_REQUIRED)),
                "{:?}",
                report.failures
            );
        }
    }

    /// Pointing the name at another repository drops each component's
    /// approval on its own, even when the questions are refused.
    #[test]
    fn a_source_change_drops_each_components_approval() {
        let (site, lock) = installed_with_manifest(MOVED_SRC, EMPTY_UI_MANIFEST);
        write_store(site.path(), Component::Agent, APPROVAL_SRC, &[FS_READ]);
        write_store(site.path(), Component::Ui, APPROVAL_SRC, &[NET]);

        let (report, _) = grant_pass(site.path(), &lock, MOVED_SRC, &Component::ALL, false);

        assert_eq!(report.packages.len(), 1, "the agent asks for nothing");
        for component in Component::ALL {
            assert!(
                read_store(site.path(), component).is_empty(),
                "{component:?} kept the old source's approval"
            );
        }
    }

    /// The read-only fallback holds the UI to the same gate, and says why a
    /// UI it cannot load is held back.
    #[test]
    fn the_fallback_reports_an_unapproved_ui() {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, UI_ONLY_MANIFEST);
        let mut failures = Vec::new();

        let found = resolved_on_disk(
            &[&declared_pack(DEMO, APPROVAL_SRC)],
            site.path(),
            &lock,
            &Component::ALL,
            &mut failures,
        );

        assert_eq!(found.len(), 1, "the agent asks for nothing");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains(UI_APPROVAL_REQUIRED), "{failures:?}");
    }

    /// `/packdel` of a package both components own revokes the components
    /// of the host that runs it, so a host that loads one leaves the other's
    /// approval alone, and keeps the checkout for the owner left.
    #[test_case(&Component::ALL, &[], None ; "both")]
    #[test_case(&[Component::Ui], &[Component::Agent], Some(&[Owner::Agent]) ; "the_ui_alone")]
    #[test_case(&[Component::Agent], &[Component::Ui], Some(&[Owner::Ui]) ; "the_agent_alone")]
    fn deleting_a_shared_package_releases_only_its_hosts_part(
        components: &'static [Component],
        kept: &[Component],
        owners_left: Option<&[Owner]>,
    ) {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        set_owners(&fixture.lock_path, &[Owner::Agent, Owner::Ui]);
        let src = format!("https://example.com/{DEMO}");
        write_store(&fixture.site, Component::Agent, &src, &[FS_READ]);
        write_store(&fixture.site, Component::Ui, &src, &[NET]);

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation(DEMO, false)],
            &[],
            &Default::default(),
            components,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed, [DEMO.to_owned()]);
        let line = match owners_left {
            Some(_) => REVIEW_RELEASE_LINE,
            None => REVIEW_DELETE_LINE,
        };
        assert!(prompt.unwrap().contains(&format!("{DEMO}: {line}")));
        for component in Component::ALL {
            assert_eq!(
                approved_in(&fixture.site, component, &src).is_some(),
                kept.contains(&component),
                "{component:?}"
            );
        }
        assert_eq!(lock_owners(&fixture.lock_path).as_deref(), owners_left);
        assert_eq!(
            maki_pack::paths::revision_dir(&fixture.site, DEMO, TEST_REV).is_dir(),
            owners_left.is_some()
        );
    }

    /// A UI package the client alone installed goes whole once nothing
    /// declares it: checkout, lock entry and UI approval.
    #[test]
    fn a_ui_host_deletes_a_package_it_alone_owns() {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        set_owners(&fixture.lock_path, &[Owner::Ui]);
        let src = format!("https://example.com/{DEMO}");
        write_store(&fixture.site, Component::Ui, &src, &[NET]);

        let (report, _) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation(DEMO, true)],
            &[],
            &Default::default(),
            &[Component::Ui],
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(lock_owners(&fixture.lock_path), None);
        assert!(!maki_pack::paths::revision_dir(&fixture.site, DEMO, TEST_REV).exists());
        assert_eq!(approved_in(&fixture.site, Component::Ui, &src), None);
    }

    /// A lock entry an older build wrote has no owners and is the agent's,
    /// so a host that loads only the UI neither deletes nor updates it.
    #[test]
    fn a_lock_entry_without_owners_is_the_agents() {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        fs::write(
            &fixture.lock_path,
            format!(
                r#"{{"packages":{{"{DEMO}":{{"src":"https://example.com/{DEMO}","rev":"{TEST_REV}"}}}}}}"#
            ),
        )
        .unwrap();

        let (report, _) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation(DEMO, true)],
            &[],
            &Default::default(),
            &[Component::Ui],
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert_eq!(
            report.failures,
            [format!("{DEMO}: {UNOWNED_DELETE_REFUSAL}")]
        );
        assert_eq!(
            lock_owners(&fixture.lock_path).as_deref(),
            Some(&[Owner::Agent][..])
        );
        assert!(maki_pack::paths::revision_dir(&fixture.site, DEMO, TEST_REV).is_dir());
    }

    /// The agent's host reviews both components of a package the agent
    /// also owns, so the client leaves its update there.
    #[test_case(&[Owner::Agent, Owner::Ui] ; "shared")]
    #[test_case(&[Owner::Agent] ; "the_agents")]
    fn a_ui_host_does_not_update_a_package_the_agent_owns(owners: &[Owner]) {
        let fixture = update_fixture_with_manifests(None, Some(UI_NET_MANIFEST));
        set_owners(&fixture.lock_path, owners);

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &[Component::Ui],
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert_eq!(
            report.failures,
            [format!("{DEMO}: {SHARED_UPDATE_REFUSAL}")]
        );
        assert!(prompt.is_none());
        let lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        assert_eq!(lock.get(DEMO).unwrap().rev, fixture.old_rev);
    }

    /// A package the UI alone owns updates from the client, which reviews
    /// and approves only its UI, and keeps its owner.
    #[test]
    fn a_ui_host_updates_a_package_it_alone_owns() {
        let fixture = update_fixture_with_manifests(None, Some(UI_NET_MANIFEST));
        set_owners(&fixture.lock_path, &[Owner::Ui]);

        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &[Component::Ui],
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains(UI_NEW_NET_LINE), "{prompt}");
        assert!(!prompt.contains("permissions: +fs_read"), "{prompt}");
        let lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        let entry = lock.get(DEMO).unwrap();
        assert_eq!(entry.rev, fixture.new_rev);
        assert_eq!(entry.owners, [Owner::Ui]);
        assert!(!fixture.site.join(APPROVALS_FILE).exists());
    }

    const AGENT_PACKAGE: &str = "agentpkg";
    const UI_PACKAGE: &str = "uipkg";

    /// A context and lockfile where {AGENT_PACKAGE} is the agent's and
    /// {UI_PACKAGE} the UI's, neither declared.
    fn owned_site() -> (SiteFixture, PackContext) {
        let fixture = managed_site(&[(AGENT_PACKAGE, TEST_REV), (UI_PACKAGE, TEST_REV)]);
        let mut lock = read_lockfile(Some(&fixture.lock_path)).unwrap();
        lock.set_owners(UI_PACKAGE, &[Owner::Ui]);
        assert!(write_lockfile(&fixture.lock_path, &lock));
        let installed = [AGENT_PACKAGE, UI_PACKAGE].map(str::to_owned).to_vec();
        let context = PackContext::new(Vec::new(), installed, Default::default());
        (fixture, context)
    }

    /// A client's commands act on its own UI packages: the agent's are left
    /// out of `++all` and refused by name, and the combined host still
    /// acts on every one.
    #[test_case(&[Component::Ui], &[UI_PACKAGE] ; "a_ui_host")]
    #[test_case(&Component::ALL, &[AGENT_PACKAGE, UI_PACKAGE] ; "the_combined_host")]
    fn delete_all_takes_the_packages_the_host_owns(components: &[Component], expected: &[&str]) {
        let (fixture, context) = owned_site();
        let command = PackCommand::Delete {
            target: DeleteTarget::Undeclared,
            force: false,
        };

        let planned = plan_owned(&command, &context, components, &fixture.lock_path).unwrap();

        let expected: Vec<PlannedOp> = expected
            .iter()
            .map(|name| delete_operation(name, false))
            .collect();
        assert_eq!(planned, expected);
    }

    #[test]
    fn a_ui_host_refuses_to_delete_the_agents_package() {
        let (fixture, context) = owned_site();
        let command = PackCommand::Delete {
            target: DeleteTarget::One(AGENT_PACKAGE.to_owned()),
            force: false,
        };

        let refused = plan_owned(&command, &context, &[Component::Ui], &fixture.lock_path);

        assert_eq!(
            refused,
            Err(format!("{AGENT_PACKAGE}: {UNOWNED_DELETE_REFUSAL}"))
        );
    }

    /// `/packdel` keeps refusing a package `ui.lua` still declares, in a
    /// client as in maki.
    #[test]
    fn a_ui_host_keeps_a_declared_package() {
        let (fixture, mut context) = owned_site();
        context.declared = vec![declared_pack(UI_PACKAGE, APPROVAL_SRC)];
        let command = PackCommand::Delete {
            target: DeleteTarget::One(UI_PACKAGE.to_owned()),
            force: false,
        };

        let refused = plan_owned(&command, &context, &[Component::Ui], &fixture.lock_path);

        assert_eq!(
            refused,
            Err(format!("{UI_PACKAGE}: {DECLARED_DELETE_REFUSAL}"))
        );
    }

    /// {name} declared in the file of {component} alone.
    fn declared_for(component: Component) -> crate::api::pack::Declared {
        let mut declared = declared_pack(DEMO, APPROVAL_SRC);
        declared
            .policies
            .retain(|&declares, _| declares == component);
        declared
    }

    /// A host adds its owner when it resolves a declaration of a package
    /// already installed, and only for the components a file declares.
    #[test_case(&[Owner::Ui], &[Component::Agent], Component::Agent, &[Owner::Agent, Owner::Ui] ; "the_agent_host")]
    #[test_case(&[Owner::Agent], &[Component::Ui], Component::Ui, &[Owner::Agent, Owner::Ui] ; "the_ui_host")]
    #[test_case(&[Owner::Ui], &Component::ALL, Component::Agent, &[Owner::Agent, Owner::Ui] ; "combined_declared_in_init_lua")]
    #[test_case(&[Owner::Ui], &[Component::Agent], Component::Ui, &[Owner::Ui] ; "a_component_the_host_does_not_load")]
    fn a_host_adds_its_owner_when_it_resolves_a_declaration(
        before: &[Owner],
        components: &[Component],
        declared_in: Component,
        after: &[Owner],
    ) {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        set_owners(&fixture.lock_path, before);
        let roots = PackRoots {
            site: fixture.site.clone(),
            lockfile: Some(fixture.lock_path.clone()),
        };

        install_declared_in(
            &roots,
            &[declared_for(declared_in)],
            Interaction::None,
            components,
            &TEST_BUNDLED,
        );

        assert_eq!(lock_owners(&fixture.lock_path).as_deref(), Some(after));
    }

    /// A UI declaration naming the agent's package from another source
    /// leaves the agent's entry and checkout alone and loads nothing.
    #[test]
    fn a_ui_declaration_from_another_source_leaves_the_agents_package() {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        let before = read_lockfile(Some(&fixture.lock_path)).unwrap();
        let roots = PackRoots {
            site: fixture.site.clone(),
            lockfile: Some(fixture.lock_path.clone()),
        };
        let mut declared = declared_for(Component::Ui);
        declared.spec = maki_pack::Spec::new(MOVED_SRC).with_name(DEMO);

        let report = install_declared_in(
            &roots,
            &[declared],
            Interaction::None,
            &[Component::Ui],
            &TEST_BUNDLED,
        );

        assert_eq!(read_lockfile(Some(&fixture.lock_path)).unwrap(), before);
        assert!(report.packages.is_empty());
        assert_eq!(
            report.failures,
            [format!("{DEMO}: {FOREIGN_SOURCE_CONFLICT}")]
        );
        assert!(maki_pack::paths::revision_dir(&fixture.site, DEMO, TEST_REV).is_dir());
    }

    /// A fresh install is its host's alone, not the agent's that an entry
    /// without owners reads as.
    #[test]
    fn a_fresh_install_is_owned_by_its_host_alone() {
        let _runtime = HOST_RUNTIME.enter();
        let temp = tempfile::TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        fs::create_dir_all(origin.join("ui")).unwrap();
        fs::write(origin.join("ui").join("init.lua"), "-- ui\n").unwrap();
        fs::write(origin.join(PLUGIN_MANIFEST), EMPTY_UI_MANIFEST).unwrap();
        run_git(&origin, &["init", "--quiet"]);
        run_git(&origin, &["config", "user.email", "test@example.com"]);
        run_git(&origin, &["config", "user.name", "test"]);
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "first"]);
        let lock_path = temp.path().join("pack-lock.json");
        let roots = PackRoots {
            site: temp.path().join("site"),
            lockfile: Some(lock_path.clone()),
        };
        let spec = maki_pack::Spec::new(origin.display().to_string()).with_name(DEMO);
        let declared = crate::api::pack::Declared::new(
            spec,
            Component::Ui,
            crate::api::pack::Policy {
                load: crate::api::pack::LoadMode::Eager,
                confirm: false,
                data: None,
            },
        );

        let report = install_declared_in(
            &roots,
            &[declared],
            Interaction::None,
            &[Component::Ui],
            &TEST_BUNDLED,
        );

        assert!(
            !mentions(&report.failures, "failed to install"),
            "{:?}",
            report.failures
        );
        assert_eq!(lock_owners(&lock_path).as_deref(), Some(&[Owner::Ui][..]));
    }

    /// The update review shows a line per component the host loads, and
    /// accepting it approves each component in its own store.
    #[test_case(&Component::ALL, true ; "both_components")]
    #[test_case(&[Component::Agent], false ; "an_agent_host")]
    fn update_review_shows_and_approves_each_component(
        components: &'static [Component],
        with_ui: bool,
    ) {
        let fixture = update_fixture_with_manifests(None, Some(UI_NET_MANIFEST));
        if with_ui {
            set_owners(&fixture.lock_path, &[Owner::Agent, Owner::Ui]);
        }
        let mut agent = fixture.declared.clone();
        agent[0]
            .policies
            .retain(|&component, _| component == Component::Agent);
        let mut context = PackContext::new(agent, vec![DEMO.into()], Default::default());
        if with_ui {
            let mut ui = fixture.declared.clone();
            ui[0]
                .policies
                .retain(|&component, _| component == Component::Ui);
            context = context
                .merge(PackContext::new(ui, vec![DEMO.into()], Default::default()))
                .unwrap();
        }
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &context.declared,
            &context.active,
            components,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");
        let src = &fixture.declared[0].spec.src;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(prompt.contains("permissions: +fs_read"), "{prompt}");
        assert_eq!(prompt.contains(UI_NEW_NET_LINE), with_ui, "{prompt}");
        assert_eq!(
            approved_in(&fixture.site, Component::Agent, src),
            Some(vec![FS_READ.to_owned()])
        );
        assert_eq!(
            fixture.site.join(UI_APPROVALS_FILE).exists(),
            with_ui,
            "only a reviewed UI is approved"
        );
        if with_ui {
            assert_eq!(
                approved_in(&fixture.site, Component::Ui, src),
                Some(vec![NET.to_owned()])
            );
        }
    }

    #[test]
    fn native_package_review_refuses_conflicting_declarations() {
        let agent = PackContext::new(
            vec![declared_pack(DEMO, APPROVAL_SRC)],
            Vec::new(),
            Default::default(),
        );
        let ui = PackContext::new(
            vec![declared_pack(DEMO, MOVED_SRC)],
            Vec::new(),
            Default::default(),
        );
        assert_eq!(
            agent.merge(ui).unwrap_err(),
            format!("{DEMO}: {CONFLICTING_DECLARATIONS}")
        );
    }

    /// A UI approved before diffs like the agent's; a manifest without
    /// `[ui]` shows and approves no UI at all.
    #[test_case(EMPTY_UI_MANIFEST, Some(UI_DROPPED_NET_LINE) ; "a_narrowed_ui")]
    #[test_case(NET_MANIFEST, None ; "no_ui_table")]
    fn update_review_diffs_a_ui_approved_before(new_manifest: &str, line: Option<&str>) {
        let fixture = update_fixture_with_manifests(Some(UI_ONLY_MANIFEST), Some(new_manifest));
        let src = fixture.declared[0].spec.src.clone();
        write_store(&fixture.site, Component::Ui, &src, &[NET]);
        let (_, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        match line {
            Some(line) => assert!(prompt.contains(line), "{prompt}"),
            None => assert!(!prompt.contains(UI_REVIEW_LINE), "{prompt}"),
        }
    }

    const MALFORMED_STORE: &str = "not json";

    fn break_ui_store(site: &Path) {
        fs::create_dir_all(site).unwrap();
        fs::write(site.join(UI_APPROVALS_FILE), MALFORMED_STORE).unwrap();
    }

    fn mentions(failures: &[String], text: &str) -> bool {
        failures.iter().any(|failure| failure.contains(text))
    }

    /// A package `ui.lua` alone declares stands on its UI store, so one
    /// that cannot be read lets nothing in, on the install pass and on the
    /// read-only fallback alike.
    #[test]
    fn a_ui_only_declaration_with_an_unreadable_ui_store_is_held_back() {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, UI_NET_MANIFEST);
        break_ui_store(site.path());
        let manager = maki_pack::manager::Manager::new(site.path());
        let mut declared = declared_pack(DEMO, APPROVAL_SRC);
        declared
            .policies
            .retain(|&declares, _| declares == Component::Ui);
        let mut report = InstallReport::default();

        grant_installed(
            &[&declared],
            site.path(),
            &manager,
            &lock,
            &Component::ALL,
            &mut |_| true,
            &mut report,
        );
        let mut failures = Vec::new();
        let on_disk = resolved_on_disk(
            &[&declared],
            site.path(),
            &lock,
            &Component::ALL,
            &mut failures,
        );

        assert!(report.packages.is_empty());
        assert!(
            mentions(&report.failures, UI_APPROVAL_STORE_UNREADABLE),
            "{:?}",
            report.failures
        );
        assert!(on_disk.is_empty(), "{failures:?}");
    }

    /// A declared package's update reviews and approves only the components
    /// a file declares.
    #[test_case(Component::Agent ; "declared_in_init_lua")]
    #[test_case(Component::Ui ; "declared_in_ui_lua")]
    fn update_review_covers_only_declared_components(component: Component) {
        let mut fixture = update_fixture_with_manifests(None, Some(UI_NET_MANIFEST));
        fixture.declared[0]
            .policies
            .retain(|&declares, _| declares == component);
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");
        let src = &fixture.declared[0].spec.src;
        let agent = component == Component::Agent;

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(prompt.contains("permissions: +fs_read"), agent, "{prompt}");
        assert_eq!(prompt.contains(UI_NEW_NET_LINE), !agent, "{prompt}");
        assert_eq!(
            approved_in(&fixture.site, Component::Agent, src).is_some(),
            agent
        );
        assert_eq!(
            approved_in(&fixture.site, Component::Ui, src).is_some(),
            !agent
        );
    }

    /// A UI file that cannot be read holds back only the UI of a package
    /// that has one, with a warning; the agent passes as it did before.
    #[test_case(NET_MANIFEST, false ; "no_ui_table")]
    #[test_case(UI_NET_MANIFEST, true ; "a_ui_table")]
    fn an_unreadable_ui_store_holds_back_only_a_ui_at_install(manifest: &str, with_ui: bool) {
        let (site, lock) = installed_with_manifest(APPROVAL_SRC, manifest);
        write_store(site.path(), Component::Agent, APPROVAL_SRC, &[NET, FS_READ]);
        break_ui_store(site.path());

        let (report, prompts) = grant_pass(site.path(), &lock, APPROVAL_SRC, &Component::ALL, true);

        assert!(prompts.is_empty(), "{prompts:?}");
        assert_eq!(report.packages.len(), 1);
        assert_eq!(
            mentions(&report.failures, UI_APPROVAL_STORE_UNREADABLE),
            with_ui
        );
        assert_eq!(
            report.failures.len(),
            usize::from(with_ui),
            "{:?}",
            report.failures
        );
        assert_eq!(
            fs::read_to_string(site.path().join(UI_APPROVALS_FILE)).unwrap(),
            MALFORMED_STORE
        );
    }

    #[test_case(NET_MANIFEST, false ; "no_ui_table")]
    #[test_case(UI_NET_MANIFEST, true ; "a_ui_table")]
    fn an_unreadable_ui_store_holds_back_only_a_ui_at_update(new_manifest: &str, with_ui: bool) {
        let fixture = update_fixture_with_manifests(None, Some(new_manifest));
        break_ui_store(&fixture.site);
        let (report, prompt) = finish_preparation(prepare_pack_ops_at(
            &[update_operation(false)],
            &fixture.declared,
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));
        let prompt = prompt.expect("a normal update needs review");

        assert_eq!(report.updated.len(), 1);
        assert!(prompt.contains("permissions: +"), "{prompt}");
        assert!(!prompt.contains(UI_REVIEW_LINE), "{prompt}");
        assert_eq!(
            mentions(&report.failures, UI_APPROVAL_STORE_UNREADABLE),
            with_ui
        );
        assert_eq!(
            report.failures.len(),
            usize::from(with_ui),
            "{:?}",
            report.failures
        );
        assert!(
            approved_in(
                &fixture.site,
                Component::Agent,
                &fixture.declared[0].spec.src
            )
            .is_some()
        );
    }

    #[test_case(NET_MANIFEST, false ; "no_ui_table")]
    #[test_case(UI_NET_MANIFEST, true ; "a_ui_table")]
    fn an_unreadable_ui_store_holds_back_only_a_ui_at_delete(manifest: &str, with_ui: bool) {
        let fixture = managed_site(&[(DEMO, TEST_REV)]);
        let checkout = maki_pack::paths::revision_dir(&fixture.site, DEMO, TEST_REV);
        fs::write(checkout.join(PLUGIN_MANIFEST), manifest).unwrap();
        let src = format!("https://example.com/{DEMO}");
        write_store(&fixture.site, Component::Agent, &src, &[NET]);
        break_ui_store(&fixture.site);

        let (report, _) = finish_preparation(prepare_pack_ops_at(
            &[delete_operation(DEMO, true)],
            &[],
            &Default::default(),
            &Component::ALL,
            &fixture.site,
            &fixture.lock_path,
            &TEST_BUNDLED,
        ));

        assert_eq!(report.removed, [DEMO.to_owned()]);
        assert_eq!(
            mentions(&report.failures, REVOKE_UI_APPROVAL_FAILURE),
            with_ui
        );
        assert_eq!(
            report.failures.len(),
            usize::from(with_ui),
            "{:?}",
            report.failures
        );
        assert_eq!(approved_in(&fixture.site, Component::Agent, &src), None);
    }
}
