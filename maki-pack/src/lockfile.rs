//! The exact commit of every managed package.
//!
//! Committing this file and running an install reproduces a package set on
//! another machine, which is the whole reason it records commits rather than
//! the `version` that produced them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The part of maki that installed or declared a package in a site, so a
/// host that loads one part never removes what the other still uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    Agent,
    Ui,
}

/// One recorded package. The source is stored alongside the revision so a
/// lockfile entry describes where its commit came from, not just what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub src: String,
    pub rev: String,
    /// Sorted. An entry without the field was written before owners
    /// existed, or by a build that drops them, and only the agent installed
    /// packages then.
    #[serde(default = "legacy_owners")]
    pub owners: Vec<Owner>,
}

fn legacy_owners() -> Vec<Owner> {
    vec![Owner::Agent]
}

/// A `BTreeMap` rather than a hash map, so the file serializes in a stable
/// order and committing it does not produce noisy diffs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    packages: BTreeMap<String, LockEntry>,
}

impl Lockfile {
    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.packages.get(name)
    }

    /// Records a package at a revision. Called only after the change on disk
    /// succeeded, so a failed fetch never moves a recorded revision.
    ///
    /// A recorded package keeps its owners, since a new revision is not a new
    /// owner; a new one reads as the agent's, as an entry from before owners
    /// did, until its host names them with [`Lockfile::set_owners`].
    pub fn record(
        &mut self,
        name: impl Into<String>,
        src: impl Into<String>,
        rev: impl Into<String>,
    ) {
        let name = name.into();
        let owners = self
            .packages
            .remove(&name)
            .map_or_else(legacy_owners, |entry| entry.owners);
        self.packages.insert(
            name,
            LockEntry {
                src: src.into(),
                rev: rev.into(),
                owners,
            },
        );
    }

    /// Replaces the owners of a recorded package; whether anything changed.
    pub fn set_owners(&mut self, name: &str, owners: &[Owner]) -> bool {
        let Some(entry) = self.packages.get_mut(name) else {
            return false;
        };
        let mut owners = owners.to_vec();
        owners.sort();
        owners.dedup();
        let changed = entry.owners != owners;
        entry.owners = owners;
        changed
    }

    /// Adds {owners} to a recorded package; whether anything changed.
    pub fn add_owners(&mut self, name: &str, owners: &[Owner]) -> bool {
        let Some(entry) = self.packages.get_mut(name) else {
            return false;
        };
        let before = entry.owners.len();
        entry.owners.extend_from_slice(owners);
        entry.owners.sort();
        entry.owners.dedup();
        entry.owners.len() != before
    }

    /// Takes {owners} off a recorded package, keeping the entry even when
    /// no owner is left: removing it goes with removing the checkout.
    pub fn remove_owners(&mut self, name: &str, owners: &[Owner]) {
        if let Some(entry) = self.packages.get_mut(name) {
            entry.owners.retain(|owner| !owners.contains(owner));
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.packages.remove(name);
    }

    /// Every recorded name, alphabetically. Pruning and `pack.get` walk the
    /// lockfile this way, so neither depends on insertion order.
    pub fn install_order(&self) -> impl Iterator<Item = &str> {
        self.packages.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(lock: &Lockfile) -> String {
        serde_json::to_string_pretty(lock).unwrap()
    }

    fn filled() -> Lockfile {
        let mut lock = Lockfile::default();
        lock.record("zebra", "https://x/zebra", "ccc");
        lock.record("alpha", "https://x/alpha", "aaa");
        lock.record("mid", "https://x/mid", "bbb");
        lock
    }

    /// Alphabetical, so what a walk of the lockfile reports is the same on
    /// every machine that shares it.
    #[test]
    fn install_order_is_alphabetical_not_insertion_order() {
        assert_eq!(
            filled().install_order().collect::<Vec<_>>(),
            ["alpha", "mid", "zebra"]
        );
    }

    #[test]
    fn round_trips_through_json() {
        let lock = filled();
        let back: Lockfile = serde_json::from_str(&json(&lock)).unwrap();
        assert_eq!(back, lock);
    }

    /// The file is meant to be committed, so the same content must serialize
    /// identically however it was built.
    #[test]
    fn serialization_is_stable_regardless_of_insertion_order() {
        let mut other = Lockfile::default();
        other.record("alpha", "https://x/alpha", "aaa");
        other.record("mid", "https://x/mid", "bbb");
        other.record("zebra", "https://x/zebra", "ccc");
        assert_eq!(json(&other), json(&filled()));
    }

    /// An entry an older build wrote, or one before owners existed, is the
    /// agent's, and an unknown owner list is never guessed at.
    #[test]
    fn an_entry_without_owners_reads_as_the_agents() {
        let lock: Lockfile =
            serde_json::from_str(r#"{"packages":{"demo":{"src":"https://x/demo","rev":"aaa"}}}"#)
                .unwrap();
        assert_eq!(lock.get("demo").unwrap().owners, [Owner::Agent]);
    }

    #[test]
    fn a_new_revision_keeps_the_owners() {
        let mut lock = filled();
        lock.set_owners("alpha", &[Owner::Ui]);
        lock.record("alpha", "https://x/alpha", "ddd");
        assert_eq!(lock.get("alpha").unwrap().owners, [Owner::Ui]);
    }

    #[test]
    fn owners_are_added_once_and_removed_alone() {
        let mut lock = filled();
        assert!(lock.add_owners("alpha", &[Owner::Ui]));
        assert!(!lock.add_owners("alpha", &[Owner::Ui, Owner::Agent]));
        lock.remove_owners("alpha", &[Owner::Ui]);
        assert_eq!(lock.get("alpha").unwrap().owners, [Owner::Agent]);
    }

    #[test]
    fn owners_round_trip_by_name() {
        let mut lock = filled();
        lock.set_owners("alpha", &[Owner::Ui, Owner::Agent]);
        let text = json(&lock);
        assert!(
            text.contains(r#""agent","#) && text.contains(r#""ui""#),
            "{text}"
        );
        let back: Lockfile = serde_json::from_str(&text).unwrap();
        assert_eq!(back, lock);
    }

    #[test]
    fn missing_packages_key_reads_as_empty() {
        let empty: Lockfile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.install_order().count(), 0);
    }
}
