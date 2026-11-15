//! itch-nfs: archives the itch.io asset packs you own (the scanner), and
//! serves the archive as a read-only NFSv3 export (the frontend).

pub mod archive;
pub mod cache;
pub mod diskcache;
pub mod fs;
pub mod ingest;
pub mod itch;
pub mod migrate;
pub mod scan;
pub mod state;
pub mod store;
pub mod tree;
