/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pippin: partition

use std::io::{Read, Write, ErrorKind};
use std::collections::{HashSet, VecDeque};
use std::result;
use std::any::Any;
use std::ops::Deref;
use std::rc::Rc;
use hashindexed::{HashIndexed, Iter};

pub use detail::states::{State, MutState, PartState, MutPartState};

use detail::readwrite::{FileHeader, UserData, FileType, read_head, write_head, validate_repo_name};
use detail::readwrite::{read_snapshot, write_snapshot};
use detail::readwrite::{read_log, start_log, write_commit};
use detail::states::{PartStateSumComparator};
use detail::{Commit, ExtraMeta, CommitQueue, LogReplay};
use merge::{TwoWayMerge, TwoWaySolver};
use {ElementT, Sum, PartId};
use error::{Result, TipError, PatchOp, MatchError, OtherError, make_io_err};

/// An interface providing read and/or write access to a suitable location.
/// 
/// Note: lifetimes on some functions are more restrictive than might seem
/// necessary; this is to allow an implementation which reads and writes to
/// internal streams.
pub trait PartIO {
    /// Convert self to a `&Any`
    fn as_any(&self) -> &Any;
    
    /// Return the partition identifier.
    fn part_id(&self) -> PartId;
    
    /// Return one greater than the snapshot number of the latest snapshot file
    /// or log file found.
    /// 
    /// The idea is that each snapshot and each set of log files can be put
    /// into a sparse vector with this length (sparse because entries may be
    /// missing; especially old entries may have been deleted).
    /// 
    /// Snapshots and commit logs with a number greater than or equal to this
    /// number probably won't exist and may in any case be ignored.
    /// 
    /// Convention: snapshot "zero" may not be an actual snapshot but
    /// either way the snapshot should be empty (no elements and the state-sum
    /// should be zero).
    /// 
    /// This number must not change except to increase when write_snapshot()
    /// is called.
    fn ss_len(&self) -> usize;
    
    /// One greater than the number of the last log file available for some snapshot
    fn ss_cl_len(&self, ss_num: usize) -> usize;
    
    /// Get a snapshot with the given number. If no snapshot is present or if
    /// ss_num is too large, None will be returned.
    /// 
    /// Returns a heap-allocated read stream, either on some external resource
    /// (such as a file) or on an internal data-structure.
    /// 
    /// This can fail due to IO operations failing.
    fn read_ss<'a>(&'a self, ss_num: usize) -> Result<Option<Box<Read+'a>>>;
    
    /// Get a commit log (numbered `cl_num`) file for a snapshot (numbered
    /// `ss_num`). If none is found, return Ok(None).
    /// 
    /// Returns a heap-allocated read stream, either on some external resource
    /// (such as a file) or on an internal data-structure.
    /// 
    /// This can fail due to IO operations failing.
    fn read_ss_cl<'a>(&'a self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Read+'a>>>;
    
    /// Open a write stream on a new snapshot file, numbered ss_num.
    /// This will increase the number returned by ss_len().
    /// 
    /// Returns None if a snapshot with number ss_num already exists.
    /// 
    /// Returns a heap-allocated write stream, either to some external resource
    /// (such as a file) or to an internal data-structure.
    /// 
    /// This can fail due to IO operations failing.
    fn new_ss<'a>(&'a mut self, ss_num: usize) -> Result<Option<Box<Write+'a>>>;
    
    /// Open an append-write stream on an existing commit file. Writes may be
    /// atomic. Each commit should be written via a single write operation.
    /// 
    /// Returns None if no commit file with this `ss_num` and `cl_num` exists.
    /// 
    /// Returns a heap-allocated write stream, either to some external resource
    /// (such as a file) or to an internal data-structure.
    /// 
    /// This can fail due to IO operations failing.
    // #0012: verify atomicity of writes
    fn append_ss_cl<'a>(&'a mut self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Write+'a>>>;
    
    /// Open a write-stream on a new commit file. As with the append version,
    /// the file will be opened in append mode, thus writes may be atomic.
    /// Each commit (and the header, including commit section marker) should be
    /// written via a single write operation.
    /// 
    /// Returns None if a commit log with number `cl_num` for snapshot `ss_num`
    /// already exists.
    /// 
    /// Returns a heap-allocated write stream, either to some external resource
    /// (such as a file) or to an internal data-structure.
    /// 
    /// This can fail due to IO operations failing.
    // #0012: verify atomicity of writes
    fn new_ss_cl<'a>(&'a mut self, ss_num: usize, cl_num: usize) -> Result<Option<Box<Write+'a>>>;
}

/// Doesn't provide any IO.
/// 
/// Can be used for testing but big fat warning: this does not provide any
/// method to save your data. Write operations fail with `ErrorKind::InvalidInput`.
pub struct DummyPartIO {
    part_id: PartId,
    // The internal buffer allows us to accept write operations. Data gets
    // written over on the next write.
    buf: Vec<u8>
}
impl DummyPartIO {
    /// Create a new instance
    pub fn new(part_id: PartId) -> DummyPartIO {
        DummyPartIO { part_id: part_id, buf: Vec::new() }
    }
}

impl PartIO for DummyPartIO {
    fn as_any(&self) -> &Any { self }
    fn part_id(&self) -> PartId { self.part_id }
    fn ss_len(&self) -> usize { 0 }
    fn ss_cl_len(&self, _ss_num: usize) -> usize { 0 }
    fn read_ss(&self, _ss_num: usize) -> Result<Option<Box<Read+'static>>> {
        Ok(None)
    }
    fn read_ss_cl(&self, _ss_num: usize, _cl_num: usize) -> Result<Option<Box<Read+'static>>> {
        Ok(None)
    }
    fn new_ss<'a>(&'a mut self, _ss_num: usize) -> Result<Option<Box<Write+'a>>> {
        self.buf.clear();
        Ok(Some(Box::new(&mut self.buf)))
    }
    fn append_ss_cl<'a>(&'a mut self, _ss_num: usize, _cl_num: usize) -> Result<Option<Box<Write+'a>>> {
        self.buf.clear();
        Ok(Some(Box::new(&mut self.buf)))
    }
    fn new_ss_cl<'a>(&'a mut self, _ss_num: usize, _cl_num: usize) -> Result<Option<Box<Write+'a>>> {
        self.buf.clear();
        Ok(Some(Box::new(&mut self.buf)))
    }
}

/// Determines when to write a new snapshot automatically.
struct SnapshotPolicy {
    commits: usize,
    edits: usize,
}
impl SnapshotPolicy {
    /// Create a new instance. Assume we have a fresh snapshot.
    fn new() -> SnapshotPolicy { SnapshotPolicy {
            commits: 0,
            edits: 0
        }
    }
    /// Report that we definitely need a new snapshot
    fn require(&mut self) { self.commits = 1000; }
    /// Report `n_commits` commits since last event.
    fn add_commits(&mut self, n_commits: usize) { self.commits += n_commits; }
    /// Report `n_edits` edits since last event.
    fn add_edits(&mut self, n_edits: usize) { self.edits += n_edits; }
    /// Report that we have a fresh snapshot
    fn reset(&mut self) {
        self.commits = 0;
        self.edits = 0;
    }
    /// Return true when we should write a snapshot
    fn snapshot(&self) -> bool { self.commits * 5 + self.edits > 150 }
}

/// A *partition* is a sub-set of the entire set such that (a) each element is
/// in exactly one partition, (b) a partition is small enough to be loaded into
/// memory in its entirety, (c) there is some user control over the number of
/// partitions and how elements are assigned partitions and (d) each partition
/// can be managed independently of other partitions.
///
/// Partitions are the *only* method by which the entire set may grow beyond
/// available memory, thus smart allocation of elements to partitions will be
/// essential for some use-cases.
/// 
/// A partition is in one of three possible states: (1) unloaded, (2) loaded
/// but requiring a merge (multiple tips), (3) ready for use.
pub struct Partition<E: ElementT> {
    // IO provider
    io: Box<PartIO>,
    // Partition name. Used to identify loaded files.
    repo_name: String,
    // Partition identifier
    part_id: PartId,
    // Number of the current snapshot file
    ss_num: usize,
    // Determines when to write new snapshots
    ss_policy: SnapshotPolicy,
    // Known committed states indexed by statesum 
    states: HashIndexed<PartState<E>, Sum, PartStateSumComparator>,
    // All states without a known successor
    tips: HashSet<Sum>,
    // Commits created but not yet saved to disk. First in at front; use as queue.
    unsaved: VecDeque<Commit<E>>,
}

// Methods creating a partition, loading its data or checking status
impl<E: ElementT> Partition<E> {
    /// Create a partition, assigning an IO provider (this can only be done at
    /// time of creation). Create a blank state in the partition, write an
    /// empty snapshot to the provided `PartIO`, and mark self as *ready
    /// for use*.
    /// 
    /// Example:
    /// 
    /// ```
    /// use pippin::{Partition, PartId};
    /// use pippin::part::DummyPartIO;
    /// 
    /// let io = Box::new(DummyPartIO::new(PartId::from_num(1)));
    /// let partition = Partition::<String>::create(io, "example repo", vec![].into());
    /// ```
    pub fn create<'a>(mut io: Box<PartIO>, name: &str,
            user_fields: Rc<Vec<UserData>>) -> Result<Partition<E>>
    {
        try!(validate_repo_name(name));
        let ss = 0;
        let part_id = io.part_id();
        info!("Creating partiton {}; writing snapshot {}", part_id, ss);
        
        let state = PartState::new(part_id);
        let header = FileHeader {
            ftype: FileType::Snapshot(0),
            name: name.to_string(),
            part_id: Some(part_id),
            user: user_fields,
        };
        if let Some(mut writer) = try!(io.new_ss(ss)) {
            try!(write_head(&header, &mut writer));
            try!(write_snapshot(&state, &mut writer));
        } else {
            return make_io_err(ErrorKind::AlreadyExists, "snapshot already exists");
        }
        
        let mut part = Partition {
            io: io,
            repo_name: header.name,
            part_id: part_id,
            ss_num: 0,
            ss_policy: SnapshotPolicy::new(),
            states: HashIndexed::new(),
            tips: HashSet::new(),
            unsaved: VecDeque::new(),
        };
        part.tips.insert(state.statesum().clone());
        part.states.insert(state);
        
        Ok(part)
    }
    
    /// Open a partition, assigning an IO provider (this can only be done at
    /// time of creation).
    /// 
    /// The partition will not be *ready to use* until data is loaded with one
    /// of the load operations. Until then most operations will fail.
    /// 
    /// If the repository name is known (e.g. from another partition), then
    /// setting this with `set_repo_name()` will ensure that the value is
    /// checked when loading files.
    /// 
    /// Example:
    /// 
    /// ```no_run
    /// use std::path::Path;
    /// use pippin::Partition;
    /// use pippin::discover;
    /// 
    /// let path = Path::new("./my-partition");
    /// let io = discover::part_from_path(path, None).unwrap();
    /// let partition = Partition::<String>::open(Box::new(io));
    /// ```
    pub fn open(io: Box<PartIO>) -> Result<Partition<E>> {
        let part_id = io.part_id();
        trace!("Opening partition {}", part_id);
        Ok(Partition {
            io: io,
            repo_name: "".to_string() /*temporary value; checked before usage elsewhere*/,
            part_id: part_id,
            ss_num: 0,
            ss_policy: SnapshotPolicy::new(),
            states: HashIndexed::new(),
            tips: HashSet::new(),
            unsaved: VecDeque::new(),
        })
    }
    
    /// Set the repo name. This is left empty by `open()`. Once set,
    /// partition operations will fail when loading a file with a different
    /// name.
    /// 
    /// This will fail if the repo name has already been set *and* is not
    /// equal to the `repo_name` parameter.
    pub fn set_repo_name(&mut self, repo_name: &str) -> Result<()> {
        if self.repo_name.len() == 0 {
            self.repo_name = repo_name.to_string();
        } else if self.repo_name != repo_name {
            return OtherError::err("repository name does not match when loading (wrong repo?)");
        }
        Ok(())
    }
    
    /// Get the repo name.
    /// 
    /// If this partition was created with `create()`, not `new()`, and no
    /// partition has been loaded yet, then this function will read a snapshot
    /// file header in order to find this name.
    /// 
    /// Returns the repo_name on success. Fails if it cannot read a header.
    pub fn get_repo_name(&mut self) -> Result<&str> {
        if self.repo_name.len() > 0 {
            return Ok(&self.repo_name);
        }
        for ss in (0 .. self.io.ss_len()).rev() {
            if let Some(mut ssf) = try!(self.io.read_ss(ss)) {
                let header = try!(read_head(&mut *ssf));
                try!(Self::verify_head(&header, &mut self.repo_name, self.part_id));
                return Ok(&self.repo_name);
            }
        }
        return OtherError::err("no snapshot found for first partition");
    }
    
    /// Load either all history available or only that required to find the
    /// latest state of the partition. Uses snapshot and log files provided by
    /// the provided `PartIO`.
    /// 
    /// If `all_history == true`, all snapshots and commits found are loaded.
    /// In this case it is possible that the history graph is not connected
    /// (i.e. it has multiple unconnected sub-graphs). If this is the case,
    /// the usual merge strategy will fail.
    /// 
    /// If the partition contains data before the load, any changes will be
    /// committed (in-memory only) and newly loaded data will be seamlessly
    /// merged with that already loaded. A merge may be required; it is also
    /// possible that tips may be part of disconnected graphs and thus
    /// unmergable as with `load_everything()`.
    /// 
    /// After the operation, the repository may be in one of three states: no
    /// known states, one directed graph of one or more states with a single
    /// tip (latest state), or a graph with multiple tips (requiring a merge
    /// operation).
    /// 
    /// Returns the header from the most recent file read. TODO: ways of getting
    /// other headers / loading files individually.
    pub fn load(&mut self, all_history: bool) -> Result<FileHeader> {
        info!("Loading partition {} data", self.part_id);
        let ss_len = self.io.ss_len();
        if ss_len == 0 {
            return make_io_err(ErrorKind::NotFound, "no snapshot files found");
        }
        let mut num = ss_len - 1;
        
        let mut header = None;
        
        // Load a snapshot (if found); return Ok(true) if successful, Ok(false)
        // if not found.
        type OptHead = Option<FileHeader>;
        let load_ss = |p: &mut Partition<E>, header: &mut OptHead, ss: usize| -> Result<bool> {
            if let Some(mut r) = try!(p.io.read_ss(ss)) {
                let head = try!(read_head(&mut r));
                let file_ver = head.ftype.ver();
                try!(Self::verify_head(&head, &mut p.repo_name, p.part_id));
                *header = Some(head);
                let state = try!(read_snapshot(&mut r, p.part_id, file_ver));
                
                p.tips.insert(state.statesum().clone());
                p.states.insert(state);
                Ok(true)
            } else { Ok(false) }
        };
        // Load all found log files for the given range of snapshot numbers
        let load_cl = |p: &mut Partition<E>, header: &mut OptHead, range| -> Result<_> {
            let mut queue = CommitQueue::new();
            for ss in range {
                for cl in 0..p.io.ss_cl_len(ss) {
                    if let Some(mut r) = try!(p.io.read_ss_cl(ss, cl)) {
                        let head = try!(read_head(&mut r));
                        try!(Self::verify_head(&head, &mut p.repo_name, p.part_id));
                        *header = Some(head);
                        try!(read_log(&mut r, &mut queue));
                    }
                }
            }
            Ok(queue)
        };
        
        if all_history {
            // All history: load all snapshots and commits in order
            let mut num_commits = 0;
            let mut num_edits = 0;
            for ss in 0..ss_len {
                try!(load_ss(self, &mut header, ss));
                
                let queue = try!(load_cl(self, &mut header, ss..(ss+1)));
                num_commits = queue.len();  // final value is number of commits after last snapshot
                let mut replayer = LogReplay::from_sets(&mut self.states, &mut self.tips);
                num_edits = try!(replayer.replay(queue));
            }
            self.ss_policy.add_commits(num_commits);
            self.ss_policy.add_edits(num_edits);
        } else {
            // Latest only: load only the latest snapshot and subsequent commits
            loop {
                if try!(load_ss(self, &mut header, num)) {
                    break;  // we stop at the most recent snapshot we find
                }
                if num == 0 {
                    // no more snapshot numbers to try; below we insert an empty state
                    // #0017: we should warn about the missing snapshot file
                    break;
                }
                num -= 1;
            }
            
            let queue = try!(load_cl(self, &mut header, num..ss_len));
            self.ss_policy.add_commits(queue.len());
            if self.tips.is_empty() {
                // Only for the case we couldn't find a snapshot file (see "num == 0" above)
                let state = PartState::new(self.part_id);
                self.tips.insert(state.statesum().clone());
                self.states.insert(state);
            }
            let mut replayer = LogReplay::from_sets(&mut self.states, &mut self.tips);
            self.ss_policy.add_edits(try!(replayer.replay(queue)));
        }
        
        self.ss_num = ss_len - 1;
        if num < ss_len -1 {
            self.ss_policy.require();
        } else {
        }
        
        if !self.tips.is_empty() {
            if let Some(head) = header {
                // success, but a merge may still be required
                return Ok(head);
            }
        }
        OtherError::err("no data loaded")
    }
    
    /// Returns true when elements have been loaded (though also see
    /// `merge_required`).
    pub fn is_loaded(&self) -> bool {
        self.tips.len() > 0
    }
    
    /// Returns true when ready for use (this is equivalent to
    /// `part.is_loaded() && !part.merge_required()`).
    pub fn is_ready(&self) -> bool {
        self.tips.len() == 1
    }
    
    /// Returns true while a merge is required.
    /// 
    /// Returns false if not ready or no tip is found as well as when a single
    /// tip is present and ready to use.
    pub fn merge_required(&self) -> bool {
        self.tips.len() > 1
    }
    
    /// Verify values in a header match those we expect.
    /// 
    /// This function is called for every file loaded. It does not take self as
    /// an argument, since it is called in situations where self.io is in use.
    pub fn verify_head(head: &FileHeader, self_name: &mut String,
        self_partid: PartId) -> Result<()>
    {
        if self_name.len() == 0 {
            *self_name = head.name.clone();
        } else if *self_name != head.name{
            return OtherError::err("repository name does not match when loading (wrong repo?)");
        }
        if let Some(h_pid) = head.part_id {
            if self_partid != h_pid {
                return OtherError::err("partition identifier differs from previous value");
            }
        }
        Ok(())
    }
    
    /// Unload data from memory. Note that unless `force == true` the operation
    /// will fail if any changes have not yet been saved to disk.
    /// 
    /// Returns true if data was unloaded, false if not (implies `!force` and 
    /// that unsaved changes exist).
    pub fn unload(&mut self, force: bool) -> bool {
        trace!("Unloading partition {} data", self.part_id);
        if force || self.unsaved.is_empty() {
            self.states.clear();
            self.tips.clear();
            true
        } else {
            false
        }
    }
    
    /// Consume the `Partition` and return the held `PartIO`.
    /// 
    /// This destroys all states held internally, but states may be cloned
    /// before unwrapping. Since `Element`s are copy-on-write, cloning
    /// shouldn't be too expensive.
    pub fn unwrap_io(self) -> Box<PartIO> {
        self.io
    }
    
    /// Get the partition's number
    pub fn part_id(&self) -> PartId {
        self.part_id
    }
}

// Methods accessing or modifying a partition's data
impl<E: ElementT> Partition<E> {
    /// Get the state-sum (key) of the tip. Fails when `tip()` fails.
    pub fn tip_key(&self) -> result::Result<&Sum, TipError> {
        if self.tips.len() == 1 {
            Ok(self.tips.iter().next().unwrap())
        } else if self.tips.is_empty() {
            Err(TipError::NotReady)
        } else {
            Err(TipError::MergeRequired)
        }
    }
    
    /// Get a reference to the PartState of the current tip. You can read
    /// this directly or make a clone in order to make your modifications.
    /// 
    /// This operation will fail if no data has been loaded yet or a merge is
    /// required.
    /// 
    /// The operation requires some copying but uses copy'c,d-on-write elements
    /// internally. This copy is needed to create a commit from the diff of the
    /// last committed state and the new state.
    pub fn tip(&self) -> result::Result<&PartState<E>, TipError> {
        Ok(&self.states.get(try!(self.tip_key())).unwrap())
    }
    
    /// Iterate over all states known. If `self.load(true)` was used to load
    /// all history available, this will include all historical states found
    /// (which may still not be all history), otherwise if `self.load(false)`
    /// was used, only some recent states (in theory, everything back to the
    /// last snapshot at time of loading) will be present.
    /// 
    /// Items are unordered (actually, they follow the order of an internal
    /// hash map, which is randomised and usually different each time the
    /// program is loaded).
    /// 
    /// NOTE: this API is may change.
    pub fn states(&self) -> StateIter<E> {
        StateIter { iter: self.states.iter(), tips: &self.tips }
    }
    
    /// Get a read-only reference to a state by its statesum, if found.
    /// 
    /// If you want to keep a copy, clone it.
    pub fn state(&self, key: &Sum) -> Option<&PartState<E>> {
        self.states.get(key)
    }
    
    /// Try to find a state given a string representation of the key (as a byte array).
    /// 
    /// Like git, we accept partial keys (so long as they uniquely resolve a key).
    pub fn state_from_string(&self, string: String) -> Result<&PartState<E>, MatchError> {
        let string = string.to_uppercase().replace(" ", "");
        let mut matching = Vec::new();
        for state in self.states.iter() {
            if state.statesum().matches_string(&string.as_bytes()) {
                matching.push(state.statesum());
            }
            if matching.len() > 1 {
                return Err(MatchError::MultiMatch(
                    matching[0].as_string(false), matching[1].as_string(false)));
            }
        }
        if matching.len() == 1 {
            Ok(self.states.get(&matching[0]).unwrap())
        } else {
            Err(MatchError::NoMatch)
        }
    }
    
    /// Merge all latest states into a single tip.
    /// 
    /// This is a convenience version of `merge_two`.
    /// 
    /// Given more than two tips, there are multiple orders in which a merge
    /// could take place, or one could in theory merge more than two tips at
    /// once. This function simply selects any two tips and merges, then
    /// repeats until done.
    pub fn merge<S: TwoWaySolver<E>>(&mut self, solver: &S) -> Result<()> {
        trace!("Partition::merge ({} tips)", self.tips.len());
        while self.tips.len() > 1 {
            let c = {
                let mut merger = try!(self.merge_two());
                merger.solve(solver);
                merger.make_commit(None)
            };
            if let Some(commit) = c {
                trace!("Pushing merge commit: {} ({} changes)", commit.statesum(), commit.num_changes());
                //FIXME: merge fails because merged state (and sum) equals one of the tips. What should we do?
                try!(self.push_commit(commit));
            } else {
                return OtherError::err("merge failed");
            }
        }
        Ok(())
    }
    
    /// Create a `TwoWayMerge` for any two tips. Use this to make a commit,
    /// then call `push_commit()`. Repeat while `self.merge_required()` holds
    /// true.
    /// 
    /// This is not eligant, but provides the user full control over the merge.
    /// Alternatively, use `self.merge(solver)`.
    pub fn merge_two(&mut self) -> Result<TwoWayMerge<E>> {
        if self.tips.len() < 2 {
            return OtherError::err("merge_two() called when no states need merging");
        }
        // TODO: order is randomised (hash security). We want this operation to
        // be reproducible, so should order tips or something.
        let (tip1, tip2) = {
            let mut iter = self.tips.iter();
            let tip1 = iter.next().unwrap();
            let tip2 = iter.next().unwrap();
            (tip1, tip2)
        };
        let common = try!(self.latest_common_ancestor(tip1, tip2));
        Ok(TwoWayMerge::new(
            self.states.get(tip1).unwrap(),
            self.states.get(tip2).unwrap(),
            self.states.get(&common).unwrap()))
    }
    
    // #0003: allow getting a reference to other states listing snapshots,
    // commits, getting non-current states and getting diffs.
    
    /// This adds a new commit to the list waiting to be written and updates
    /// the states and 'tips' stored internally by creating a new state from
    /// the commit.
    /// 
    /// Fails if there is a checksum collision or the patch does not apply.
    /// 
    /// TODO: this operation should not fail, since failure might result in
    /// data loss.
    pub fn push_commit(&mut self, commit: Commit<E>) -> Result<(), PatchOp> {
        if self.states.contains(commit.statesum()) {
            return Err(PatchOp::SumClash);
        }
        let first_parent = commit.parents().iter().next().expect("commit 1st parent").clone();
        let state = {
            let parent = try!(self.states.get(&first_parent).ok_or(PatchOp::NoParent));
            try!(commit.apply(parent))
        };
        self.add_pair(commit, state);
        Ok(())
    }
    
    /// This creates a commit from the given state, converts the `MutPartState`
    /// to a `PartState` and adds it to the list of internal states, as
    /// the new tip (unless a merge is required, in which case it will be one
    /// of multiple tip states). The commit is added to the internal list
    /// waiting to be written to permanent storage (see `write()`).
    /// 
    /// Returns `Ok(true)` on success, or `Ok(false)` if the state matches its
    /// parent (i.e. hasn't been changed).
    /// 
    /// We assume there are no extra parents; merges should be pushed via
    /// `push_commit` instead.
    /// 
    /// TODO: this operation should not fail, since failure might result in
    /// data loss.
    pub fn push_state(&mut self, state: MutPartState<E>,
            extra_meta: ExtraMeta) -> Result<bool, PatchOp>
    {
        let c = {
            let parent = try!(self.states.get(&state.parent()).ok_or(PatchOp::NoParent));
            if parent.statesum() ^ &parent.metasum() == *state.elt_sum() {
                // Checksum equals that of parent: no changes
                // #0022: compare states instead of sums to check for collisions?
                None
            } else {
                // #0019: Commit::from_diff compares old and new states and code be slow.
                // #0019: Instead, we could record each alteration as it happens.
                Commit::from_diff(parent, &state, extra_meta)
            }
        };
        
        if let Some(commit) = c {
            let new_state = PartState::from_mut(state,
                    commit.parents().clone(), commit.meta().clone());
            self.add_pair(commit, new_state);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    
    /// This will write all unsaved commits to a log on the disk.
    /// 
    /// If `fast` is true, no further actions will happen, otherwise required
    /// maintenance operations will be carried out (e.g. creating a new
    /// snapshot when the current commit-log is long).
    /// 
    /// Either way, this does nothing if no changes have been made and nothing
    /// is loaded. If data has been loaded but no changes made it is still
    /// possible that a snapshot will be written (when `fast == false`).
    /// 
    /// Returns true if any commits were written (i.e. unsaved commits
    /// were found). Returns false if nothing needed doing.
    /// 
    /// Note that writing to disk can fail. In this case it may be worth trying
    /// again.
    pub fn write(&mut self, fast: bool, user_fields: Rc<Vec<UserData>>) -> Result<bool> {
        // First step: write commits
        let has_changes = !self.unsaved.is_empty();
        if has_changes {
            trace!("Partition {}: writing {} commits to log",
                self.part_id, self.unsaved.len());
            
            // #0012: extend existing logs instead of always writing a new log file.
            let mut cl_num = self.io.ss_cl_len(self.ss_num);
            loop {
                if let Some(mut writer) = try!(self.io.new_ss_cl(self.ss_num, cl_num)) {
                    // Write a header since this is a new file:
                    let header = FileHeader {
                        ftype: FileType::CommitLog(0),
                        name: self.repo_name.clone(),
                        part_id: Some(self.part_id),
                        user: user_fields.clone(),
                    };
                    try!(write_head(&header, &mut writer));
                    try!(start_log(&mut writer));
                    
                    // Now write commits:
                    while !self.unsaved.is_empty() {
                        // We try to write the commit, then when successful remove it
                        // from the list of 'unsaved' commits.
                        try!(write_commit(&self.unsaved.front().unwrap(), &mut writer));
                        self.unsaved.pop_front().expect("pop_front");
                    }
                    break;
                } else {
                    // Log file already exists! So try another number.
                    if cl_num > 1000_000 {
                        // We should give up eventually. When is arbitrary.
                        return Err(box OtherError::new("Commit log number too high"));
                    }
                    cl_num += 1;
                }
            }
        }
        
        // Second step: maintenance operations
        if !fast {
            if self.is_ready() && self.ss_policy.snapshot() {
                try!(self.write_snapshot(user_fields));
            }
        }
        
        Ok(has_changes)
    }
    
    /// Write a new snapshot from the tip.
    /// 
    /// Normally you can just call `write()` and let the library figure out
    /// when to write a new snapshot, though you can also call this directly.
    /// 
    /// Does nothing when `tip()` fails (returning `Ok(())`).
    pub fn write_snapshot(&mut self, user_fields: Rc<Vec<UserData>>) -> Result<()> {
        // fail early if not ready:
        let tip_key = try!(self.tip_key()).clone();
        
        let mut ss_num = self.ss_num + 1;
        loop {
            // Try to get a writer for this snapshot number:
            if let Some(mut writer) = try!(self.io.new_ss(ss_num)) {
                info!("Partition {}: writing snapshot {}: {}",
                    self.part_id, ss_num, tip_key);
                
                let header = FileHeader {
                    ftype: FileType::Snapshot(0),
                    name: self.repo_name.clone(),
                    part_id: Some(self.part_id),
                    user: user_fields,
                };
                //TODO: also write classifier stuff
                try!(write_head(&header, &mut writer));
                try!(write_snapshot(self.states.get(&tip_key).unwrap(), &mut writer));
                self.ss_num = ss_num;
                self.ss_policy.reset();
                return Ok(())
            } else {
                // Snapshot file already exists! So try another number.
                if ss_num > 1000_000 {
                    // We should give up eventually. When is arbitrary.
                    return Err(box OtherError::new("Snapshot number too high"));
                }
                ss_num += 1;
            }
        }
    }
}

// Internal support functions
impl<E: ElementT> Partition<E> {
    // Take self and two sums. Return a copy of a key to avoid lifetime issues.
    // 
    // TODO: enable loading of additional history on demand. Or do we not need
    // this?
    fn latest_common_ancestor(&self, k1: &Sum, k2: &Sum) -> Result<Sum> {
        // #0019: there are multiple strategies here; we just find all
        // ancestors of one, then of the other. This simplifies lopic.
        let mut a1 = HashSet::new();
        
        let mut next = VecDeque::new();
        next.push_back(k1);
        loop {
            let k = match next.pop_back() {
                Some(k) => k,
                None => { break; }
            };
            if a1.contains(k) { continue; }
            a1.insert(k);
            if let Some(state) = self.states.get(k) {
                for p in state.parents() {
                    next.push_back(p);
                }
            }
        }
        
        // We track ancestors of k2 just to check we don't end up in a loop.
        let mut a2 = HashSet::new();
        
        // next is empty
        next.push_back(k2);
        loop {
            let k = match next.pop_back() {
                Some(k) => k,
                None => { break; }
            };
            if a2.contains(k) { continue; }
            a2.insert(k);
            if a1.contains(k) {
                return Ok(k.clone());
            }
            if let Some(state) = self.states.get(k) {
                for p in state.parents() {
                    next.push_back(p);
                }
            }
        }
        
        Err(box OtherError::new("unable to find a common ancestor"))
    }
    
    // Add a paired commit and state.
    // Assumptions: checksums match and parent state is present.
    fn add_pair(&mut self, commit: Commit<E>, state: PartState<E>) {
        assert_eq!(commit.parents(), state.parents());
        assert_eq!(commit.statesum(), state.statesum());
        trace!("Partition {}: new commit {}", self.part_id, commit.statesum());
        self.ss_policy.add_commits(1);
        self.ss_policy.add_edits(commit.num_changes());
        self.unsaved.push_back(commit);
        // This might fail (if the parent was not a tip), but it doesn't matter:
        for parent in state.parents() {
            self.tips.remove(parent);
        }
        self.tips.insert(state.statesum().clone());
        self.states.insert(state);
    }
}

/// Wrapper around a `PartState<E>`. Dereferences to this type.
pub struct StateItem<'a, E: ElementT+'a> {
    state: &'a PartState<E>,
    tips: &'a HashSet<Sum>,
}
impl<'a, E: ElementT+'a> StateItem<'a, E> {
    /// Returns true if and only if this state is a tip state (i.e. is not the
    /// parent of any other state).
    /// 
    /// There is exactly one tip state unless a merge is required or no states
    /// are loaded.
    pub fn is_tip(&self) -> bool {
        self.tips.contains(self.state.statesum())
    }
}
impl<'a, E: ElementT+'a> Deref for StateItem<'a, E> {
    type Target = PartState<E>;
    fn deref(&self) -> &Self::Target {
        self.state
    }
}

/// Iterator over a partition's (historical or current) states
pub struct StateIter<'a, E: ElementT+'a> {
    iter: Iter<'a, PartState<E>, Sum, PartStateSumComparator>,
    tips: &'a HashSet<Sum>,
}
impl<'a, E: ElementT+'a> Iterator for StateIter<'a, E> {
    type Item = StateItem<'a, E>;
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|item|
            StateItem {
                state: item,
                tips: self.tips,
            }
        )
    }
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}


#[test]
fn on_new_partition() {
    let io = box DummyPartIO::new(PartId::from_num(7));
    let mut part = Partition::<String>::create(io, "on_new_partition", vec![].into())
            .expect("partition creation");
    assert_eq!(part.tips.len(), 1);
    
    let state = part.tip().expect("getting tip").clone_mut();
    assert_eq!(part.push_state(state, None).expect("committing"), false);
    
    let mut state = part.tip().expect("getting tip").clone_mut();
    assert!(!state.any_avail());
    
    let elt1 = "This is element one.".to_string();
    let elt2 = "Element two data.".to_string();
    let e1id = state.insert(elt1).expect("inserting elt");
    let e2id = state.insert(elt2).expect("inserting elt");
    
    assert_eq!(part.push_state(state, None).expect("comitting"), true);
    assert_eq!(part.unsaved.len(), 1);
    assert_eq!(part.states.len(), 2);
    let key = part.tip().expect("tip").statesum().clone();
    {
        let state = part.state(&key).expect("getting state by key");
        assert!(state.is_avail(e1id));
        assert_eq!(state.get(e2id), Ok(&"Element two data.".to_string()));
    }   // `state` goes out of scope
    assert_eq!(part.tips.len(), 1);
    let state = part.tip().expect("getting tip").clone_mut();
    assert_eq!(*state.parent(), key);
    
    assert_eq!(part.push_state(state, None).expect("committing"), false);
}