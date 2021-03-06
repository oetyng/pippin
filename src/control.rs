/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pippin: control traits

use std::usize;
use std::marker::PhantomData;

use commit::MakeCommitMeta;
use elt::Element;
use error::Result;
use io::RepoIO;
use rw::header::{UserData, FileHeader};


/// Allows the user to control various repository operations. Library-provided implementations
/// should be sufficient for many use-cases, but can be overridden or replaced if necessary.
/// 
/// Pippin data files allow arbitrary *user fields* in the headers; these can be set and read on
/// file creation / loading.
/// 
/// Each commit carries metadata: a timestamp and an "extra metadata" field; these can be set
/// by the user. (They can be read by retrieving and examining a `Commit`).
pub trait Control: MakeCommitMeta {
    /// User-defined type of elements stored
    type Element: Element;
    
    /// Get access to an I/O provider.
    /// 
    /// This layer of indirection allows use of `RepoFileIO`, which should be sufficient
    /// for many use cases.
    fn io(&self) -> &RepoIO;
    
    /// Get mutable access to an I/O provider.
    fn io_mut(&mut self) -> &mut RepoIO;
    
    /// Get access to the snapshot policy.
    /// 
    /// This layer of indirection allows use of the `DefaultSnapshot`.
    fn snapshot_policy(&mut self) -> &mut SnapshotPolicy;
    
    /// Cast self to a `&MakeCommitMeta`
    // #0018: shouldn't be needed when Rust finally supports upcasting
    fn as_mcm_ref(&self) -> &MakeCommitMeta;
    
    /// Cast self to a `&mut MakeCommitMeta`
    // #0018: shouldn't be needed when Rust finally supports upcasting
    fn as_mcm_ref_mut(&mut self) -> &mut MakeCommitMeta;
    
    // #0040: inform user of file name and/or SS&CL numbers when reading/writing user data
    
    /// This function allows population of the *user fields* of a header. This function is passed
    /// a reference to a `FileHeader` struct, where all fields have been set excepting `user`,
    /// the user fields (this should be an empty container). This function should return a set of
    /// user data to be added to the `FileHeader`.
    /// 
    /// The partition identifier and file type can be read from the passed `FileHeader`.
    /// 
    /// Returning an error will abort creation of the corresponding file.
    /// 
    /// The default implementation does not make any user data (returns an empty `Vec`).
    fn make_user_data(&mut self, _header: &FileHeader) -> Result<Vec<UserData>> {
        Ok(vec![])
    }
    
    /// This function allows the user to read data from a header when a file is loaded.
    /// 
    /// Returning an error will abort reading of this file.
    /// 
    /// The default implementation does nothing.
    fn read_header(&mut self, _header: &FileHeader) -> Result<()> {
        Ok(())
    }
}

/// An interface allowing configuration of snapshot policy.
/// 
/// It is assumed that one or more internal counters are incremented when `count` is called and
/// used to determine when `want_snapshot` returns true.
pub trait SnapshotPolicy {
    /// Reset internal counters (we have an up-to-date snapshot).
    fn reset(&mut self);
    
    /// Declare that a snapshot is required (i.e. force `want_snapshot` to be true until `reset` is
    /// next called).
    fn force_snapshot(&mut self);
    
    /// Increment an internal counter/counters to record this many `commits` and `edits`.
    fn count(&mut self, commits: usize, edits: usize);
    
    /// Defines our snapshot policy: this should return true when a new snapshot is required.
    /// 
    /// The number of commits and edits saved since the last snapshot 
    /// The default implementation is
    /// ```rust
    /// commits * 5 + edits > 150
    /// ```
    fn want_snapshot(&self) -> bool;
}

/// A convenient implementation of `Control`.
/// 
/// Uses `DefaultSnapshot` snapshot policy.
#[derive(Debug)]
pub struct DefaultControl<E: Element, IO: RepoIO + 'static> {
    _elt_type: PhantomData<E>,
    io: IO,
    ss_policy: DefaultSnapshot,
}
impl<E: Element, IO: RepoIO> DefaultControl<E, IO> {
    /// Create, given I/O provider
    pub fn new(io: IO) -> Self {
        DefaultControl { _elt_type: Default::default(), io: io, ss_policy: Default::default() }
    }
    
    /// Get direct access to the held `IO`
    pub fn io(&self) -> &IO { &self.io }
    /// Get direct mutable access to the held `IO`
    pub fn io_mut(&mut self) -> &mut IO { &mut self.io }
    /// Unwrap the held `IO`
    pub fn unwrap_io(self) -> IO { self.io }
}
impl<E: Element, IO: RepoIO> MakeCommitMeta for DefaultControl<E, IO> {}
impl<E: Element, IO: RepoIO> Control for DefaultControl<E, IO> {
    type Element = E;
    fn io(&self) -> &RepoIO {
        &self.io
    }
    fn io_mut(&mut self) -> &mut RepoIO {
        &mut self.io
    }
    fn snapshot_policy(&mut self) -> &mut SnapshotPolicy {
        &mut self.ss_policy
    }
    fn as_mcm_ref(&self) -> &MakeCommitMeta { self }
    fn as_mcm_ref_mut(&mut self) -> &mut MakeCommitMeta { self }
}

/// Default snapshot policy: snapshot when `commits * 5 + edits > 150`.
/// 
/// Can be constructed with `Default`.
#[derive(Debug, Default)]
pub struct DefaultSnapshot {
    counter: usize,
}

impl SnapshotPolicy for DefaultSnapshot {
    fn reset(&mut self) {
        self.counter = 0;
    }
    
    fn force_snapshot(&mut self) {
        self.counter = 1000;
    }
    
    fn count(&mut self, commits: usize, edits: usize) {
        self.counter += commits * 5 + edits;
    }
    
    fn want_snapshot(&self) -> bool {
        self.counter > 150
    }
}
