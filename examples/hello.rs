// The obligatory hello-world example.

use pippin::{discover, fileio, Partition, State, MutState, PartId, Result};

extern crate pippin;

// Main function — for error handling
fn inner() -> Result<()> {
    // We try to find Pippin files in '.':
    println!("Looking for Pippin files in the current directory ...");
    match discover::part_from_path(".", None) {
        Ok(io) => {
            // Read the found files:
            let mut part = try!(Partition::<String>::open(Box::new(io)));
            try!(part.load_latest(None));
            
            // Get access to the latest state:
            let tip = try!(part.tip());
            println!("Found {} element(s)", tip.num_avail());
            
            // Read the elements (API may change here):
            for (id, elt) in tip.elt_map().iter() {
                println!("Element {}: {}", id, *elt);
            }
        },
        Err(e) => {
            println!("Error: {}", e);
            println!("Creating a new partition instead");
            
            // Create a new partition:
            // PartFileIO is a dumb file accessor; hence needing to specify PartId. This may change.
            let io = Box::new(fileio::PartFileIO::new_empty(PartId::from_num(1), "hello"));
            
            let mut part = try!(Partition::create(io, "hello world", None));
            
            // Create a new state derived from the tip:
            let mut state = try!(part.tip()).clone_mut();
            try!(state.insert("Hello, world!".to_string()));
            try!(part.push_state(state, None));
            
            // Write our changes:
            try!(part.write(false, None));
        }
    }
    Ok(())
}

fn main() {
    match inner() {
        Ok(()) => {},
        Err(e) => {
            println!("Error: {}", e);
        }
    }
}