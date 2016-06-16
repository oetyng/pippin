/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pippin change/commit log reading and writing

//! Support for reading and writing Rust snapshots

use std::io::{Read, Write};
use std::collections::HashMap;
use std::rc::Rc;
use std::u32;

use byteorder::{ByteOrder, BigEndian, WriteBytesExt};

use readwrite::sum;
use commit::{Commit, EltChange, CommitMeta, ExtraMeta};
use {ElementT, Sum};
use sum::BYTES as SUM_BYTES;
use error::{Result, ReadError};

/// Implement this to use read_log().
/// 
/// There is a simple implementation for `Vec<Commit<E>>` which just pushes
/// each commit and returns `true` (to continue reading to the end).
pub trait CommitReceiver<E: ElementT> {
    /// Implement to receive a commit once it has been read. Return true to
    /// continue reading or false to stop reading more commits.
    fn receive(&mut self, commit: Commit<E>) -> bool;
}
impl<E: ElementT> CommitReceiver<E> for Vec<Commit<E>> {
    /// Implement function required by readwrite::read_log().
    fn receive(&mut self, commit: Commit<E>) -> bool {
        self.push(commit);
        true    // continue reading to EOF
    }
}


/// Read a commit log from a stream
pub fn read_log<E: ElementT>(mut reader: &mut Read,
        receiver: &mut CommitReceiver<E>) -> Result<()>
{
    let mut pos: usize = 0;
    let mut buf = vec![0; 32];
    
    try!(reader.read_exact(&mut buf[0..16]));
    if buf[0..16] != *b"COMMIT LOG\x00\x00\x00\x00\x00\x00" {
        return ReadError::err("unexpected contents (expected \
            COMMIT LOG\\x00\\x00\\x00\\x00\\x00\\x00)", pos, (0, 16));
    }
    pos += 16;
    
    // We now read commits. Since new commits can simply be appended to the
    // file, we only know we're at the end if we hit EOF. This is the only
    // condition where encountering EOF is not an error.
    loop {
        // A reader which calculates the checksum of what was read:
        let mut r = sum::HashReader::new(reader);
        
        let l = try!(r.read(&mut buf[0..16]));
        if l == 0 { break; /*end of file (EOF)*/ }
        if l < 16 { try!(r.read_exact(&mut buf[l..16])); /*not EOF, buf haven't filled buffer*/ }
        
        let n_parents = if buf[0..6] == *b"COMMIT" {
            1
        } else if buf[0..5] == *b"MERGE" {
            let n: u8 = buf[5];
            if n < 2 { return ReadError::err("bad number of parents", pos, (5, 6)); }
            n as usize
        } else {
            return ReadError::err("unexpected contents (expected COMMIT or MERGE)", pos, (0, 6));
        };
        if buf[6..8] != *b"\x00U" {
            return ReadError::err("unexpected contents (expected \\x00U)", pos, (6, 8));
        }
        let secs = BigEndian::read_i64(&buf[8..16]);
        pos += 16;
        
        try!(r.read_exact(&mut buf[0..16]));
        if buf[0..4] != *b"CNUM" {
            return ReadError::err("unexpected contents (expected CNUM)", pos, (0, 4));
        }
        let cnum = BigEndian::read_u32(&buf[4..8]);
        
        if buf[8..10] != *b"XM" {
            return ReadError::err("unexpected contents (expected XM)", pos, (8, 10));
        }
        let xm_type_txt = buf[10..12] == *b"TT";
        let xm_len = BigEndian::read_u32(&buf[12..16]) as usize;
        pos += 16;
        
        let mut xm_data = vec![0; xm_len];
        try!(r.read_exact(&mut xm_data));
        let xm = if xm_type_txt {
            ExtraMeta::Text(try!(String::from_utf8(xm_data)
                .map_err(|_| ReadError::new("content not valid UTF-8", pos, (0, xm_len)))))
        } else {
            // even if xm_len > 0 we ignore it
            ExtraMeta::None
        };
        
        pos += xm_len;
        let pad_len = 16 * ((xm_len + 15) / 16) - xm_len;
        if pad_len > 0 {
            try!(r.read_exact(&mut buf[0..pad_len]));
            pos += pad_len;
        }
        
        let meta = CommitMeta::new_explicit(cnum, secs, xm);
        
        let mut parents = Vec::with_capacity(n_parents);
        for _ in 0..n_parents {
            try!(r.read_exact(&mut buf[0..SUM_BYTES]));
            parents.push(Sum::load(&buf[0..SUM_BYTES]));
            pos += SUM_BYTES;
        }
        
        try!(r.read_exact(&mut buf[0..16]));
        if buf[0..8] != *b"ELEMENTS" {
            return ReadError::err("unexpected contents (expected ELEMENTS)", pos, (0, 8));
        }
        let num_elts = BigEndian::read_u64(&buf[8..16]) as usize;   // #0015
        pos += 16;
        
        let mut changes = HashMap::new();
        
        for _ in 0..num_elts {
            try!(r.read_exact(&mut buf[0..16]));
            if buf[0..4] != *b"ELT " {
                return ReadError::err("unexpected contents (expected ELT\\x20)", pos, (0, 4));
            }
            let elt_id = BigEndian::read_u64(&buf[8..16]).into();
            let change_t = match &buf[4..8] {
                b"DEL\x00" => { Change::Delete },
                b"INS\x00" => { Change::Insert },
                b"REPL" => { Change::Replace },
                b"MOVO" => { Change::MovedOut },
                b"MOV\x00" => { Change::Moved },
                _ => {
                    return ReadError::err("unexpected contents (expected one \
                        of DEL\\x00, INS\\x00, REPL)", pos, (4, 8));
                }
            };
            pos += 16;
            
            let change = match change_t {
                Change::Delete => EltChange::deletion(),
                Change::Insert | Change::Replace => {
                    try!(r.read_exact(&mut buf[0..16]));
                    if buf[0..8] != *b"ELT DATA" {
                        return ReadError::err("unexpected contents (expected ELT DATA)", pos, (0, 8));
                    }
                    let data_len = BigEndian::read_u64(&buf[8..16]) as usize;   // #0015
                    pos += 16;
                    
                    let mut data = vec![0; data_len];
                    try!(r.read_exact(&mut data));
                    pos += data_len;
                    
                    let pad_len = 16 * ((data_len + 15) / 16) - data_len;
                    if pad_len > 0 {
                        try!(r.read_exact(&mut buf[0..pad_len]));
                        pos += pad_len;
                    }
                    
                    let elt_sum = Sum::elt_sum(elt_id, &data);
                    try!(r.read_exact(&mut buf[0..SUM_BYTES]));
                    if !elt_sum.eq(&buf[0..SUM_BYTES]) {
                        return ReadError::err("element checksum mismatch", pos, (0, SUM_BYTES));
                    }
                    pos += SUM_BYTES;
                    
                    let elt = Rc::new(try!(E::from_vec_sum(data, elt_sum)));
                    match change_t {
                        Change::Insert => EltChange::insertion(elt),
                        Change::Replace => EltChange::replacement(elt),
                        _ => panic!()
                    }
                },
                Change::MovedOut | Change::Moved => {
                    try!(r.read_exact(&mut buf[0..16]));
                    if buf[0..8] != *b"NEW ELT\x00" {
                        return ReadError::err("unexpected contents (expected NEW ELT)", pos, (0, 8));
                    }
                    let new_id = BigEndian::read_u64(&buf[8..16]).into();
                    EltChange::moved(new_id, change_t == Change::MovedOut)
                }
            };
            changes.insert(elt_id, change);
        }
        
        try!(r.read_exact(&mut buf[0..SUM_BYTES]));
        let commit_sum = Sum::load(&buf[0..SUM_BYTES]);
        pos += SUM_BYTES;
        
        let sum = r.sum();
        reader = r.into_inner();
        try!(reader.read_exact(&mut buf[0..SUM_BYTES]));
        if !sum.eq(&buf[0..SUM_BYTES]) {
            return ReadError::err("checksum invalid", pos, (0, SUM_BYTES));
        }
        
        trace!("Read commit ({} changes): {}; first parent: {}", changes.len(), commit_sum, parents[0]);
        let cont = receiver.receive(Commit::new_explicit(commit_sum, parents, changes, meta));
        if !cont { break; }
    }
    
    #[derive(Eq, PartialEq, Copy, Clone, Debug)]
    enum Change {
        Delete, Insert, Replace, MovedOut, Moved
    }
    
    Ok(())
}

/// Write the section identifier at the start of a commit log
// #0016: do we actually need this?
pub fn start_log(writer: &mut Write) -> Result<()> {
    try!(writer.write(b"COMMIT LOG\x00\x00\x00\x00\x00\x00"));
    Ok(())
}

/// Write a single commit to a stream
pub fn write_commit<E: ElementT>(commit: &Commit<E>, writer: &mut Write) -> Result<()> {
    trace!("Writing commit ({} changes): {}",
        commit.num_changes(), commit.statesum());
    
    // A writer which calculates the checksum of what was written:
    let mut w = sum::HashWriter::new(writer);
    
    if commit.parents().len() == 1 {
        try!(w.write(b"COMMIT\x00U"));
    } else {
        assert!(commit.parents().len() > 1 && commit.parents().len() < 0x100);
        try!(w.write(b"MERGE"));
        let n: [u8; 1] = [commit.parents().len() as u8];
        try!(w.write(&n));
        try!(w.write(b"\x00U"));
    }
    
    try!(w.write_i64::<BigEndian>(commit.meta().timestamp()));
    
    try!(w.write(b"CNUM"));
    try!(w.write_u32::<BigEndian>(commit.meta().number()));
    
    match commit.meta().extra() {
        &ExtraMeta::None => {
            // last four zeros is 0u32 encoded in bytes
            try!(w.write(b"XM\x00\x00\x00\x00\x00\x00"));
        },
        &ExtraMeta::Text(ref txt) => {
            try!(w.write(b"XMTT"));
            assert!(txt.len() <= u32::MAX as usize);
            try!(w.write_u32::<BigEndian>(txt.len() as u32));
            try!(w.write(txt.as_bytes()));
            let pad_len = 16 * ((txt.len() + 15) / 16) - txt.len();
            if pad_len > 0 {
                let padding = [0u8; 15];
                try!(w.write(&padding[0..pad_len]));
            }
        },
    }
    
    // Parent statesums (we wrote the number above already):
    for parent in commit.parents() {
        try!(parent.write(&mut w));
    }
    
    try!(w.write(b"ELEMENTS"));
    try!(w.write_u64::<BigEndian>(commit.num_changes() as u64));       // #0015
    
    let mut elt_buf = Vec::new();
    
    for (elt_id,change) in commit.changes_iter() {
        let marker = match change {
            &EltChange::Deletion => b"ELT DEL\x00",
            &EltChange::Insertion(_) => b"ELT INS\x00",
            &EltChange::Replacement(_) => b"ELT REPL",
            &EltChange::MovedOut(_) => b"ELT MOVO",
            &EltChange::Moved(_) => b"ELT MOV\x00",
        };
        try!(w.write(marker));
        try!(w.write_u64::<BigEndian>((*elt_id).into()));
        if let Some(elt) = change.element() {
            try!(w.write(b"ELT DATA"));
            elt_buf.clear();
            try!(elt.write_buf(&mut &mut elt_buf));
            try!(w.write_u64::<BigEndian>(elt_buf.len() as u64));      // #0015
            
            try!(w.write(&elt_buf));
            let pad_len = 16 * ((elt_buf.len() + 15) / 16) - elt_buf.len();
            if pad_len > 0 {
                let padding = [0u8; 15];
                try!(w.write(&padding[0..pad_len]));
            }
            
            try!(elt.sum(*elt_id).write(&mut w));
        }
        if let Some(new_id) = change.moved_id() {
            try!(w.write(b"NEW ELT\x00"));
            try!(w.write_u64::<BigEndian>(new_id.into()));
        }
    }
    
    try!(commit.statesum().write(&mut w));
    
    let sum = w.sum();
    try!(sum.write(&mut w.into_inner()));
    
    Ok(())
}

#[test]
fn commit_write_read(){
    use PartId;
    
    // Note that we can make up completely nonsense commits here. Element
    // checksums must still match but state sums don't need to since we won't
    // be reproducing states. So lets make some fun sums!
    let mut v: Vec<u8> = (0u8..).take(SUM_BYTES).collect();
    let seq = Sum::load(&v);
    v = (0u8..).map(|x| x.wrapping_mul(x)).take(SUM_BYTES).collect();
    let squares = Sum::load(&v);
    v = (1u8..).map(|x| x.wrapping_add(7u8).wrapping_mul(3u8)).take(SUM_BYTES).collect();
    let nonsense = Sum::load(&v);
    v = (1u8..).map(|x| x.wrapping_mul(x).wrapping_add(5u8.wrapping_mul(x)).wrapping_add(11u8)).take(SUM_BYTES).collect();
    let quadr = Sum::load(&v);
    
    let p = PartId::from_num(1681);
    let mut changes = HashMap::new();
    changes.insert(p.elt_id(3), EltChange::insertion(Rc::new("three".to_string())));
    changes.insert(p.elt_id(4), EltChange::insertion(Rc::new("four".to_string())));
    changes.insert(p.elt_id(5), EltChange::insertion(Rc::new("five".to_string())));
    let meta1 = CommitMeta::new_explicit(1, 123456, ExtraMeta::None);
    let commit_1 = Commit::new_explicit(seq, vec![squares], changes, meta1);
    
    changes = HashMap::new();
    changes.insert(p.elt_id(1), EltChange::deletion());
    changes.insert(p.elt_id(9), EltChange::replacement(Rc::new("NINE!".to_string())));
    changes.insert(p.elt_id(5), EltChange::insertion(Rc::new("five again?".to_string())));
    let meta2 = CommitMeta::new_explicit(1, 321654, ExtraMeta::Text("123".to_string()));
    let commit_2 = Commit::new_explicit(nonsense, vec![quadr], changes, meta2);
    
    let mut obj = Vec::new();
    assert!(start_log(&mut obj).is_ok());
    assert!(write_commit(&commit_1, &mut obj).is_ok());
    assert!(write_commit(&commit_2, &mut obj).is_ok());
    
    let mut commits = Vec::new();
    match read_log(&mut &obj[..], &mut commits) {
        Ok(()) => {},
        Err(e) => {
//             // specialisation for a ReadError:
//             panic!("read_log failed: {}", e.display(&obj));
            panic!("read_log failed: {}", e);
        }
    }
    
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0], commit_1);
    assert_eq!(commits[1], commit_2);
}