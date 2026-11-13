//! The directory tree of one pack version, with cross-version de-duplication.

use std::collections::HashMap;

use anyhow::{Result, bail};
use bytes::Bytes;

use crate::archive::{Kind, Manifest};

/// Virtual file at the top of every version, listing what was hidden and where
/// the earliest copy lives.
pub const DEDUP_FILE: &str = ".itch-nfs-deduplicated.tsv";

pub const MAX_NODES: usize = 0xFFF_FFFF;

/// An asset's identity across versions: file name, size and CRC-32. The
/// directory is deliberately not part of it, so that assets survive a pack
/// being re-zipped under a different top-level folder.
type DedupKey = (String, u64, u32);

/// Every asset in the versions processed so far -> where it first appeared.
#[derive(Default)]
pub struct Seen(HashMap<DedupKey, String>);

pub struct Node {
    pub name: String,
    pub parent: u32,
    /// False for files that duplicate an earlier version, and for directories
    /// left with nothing visible in them.
    pub visible: bool,
    pub kind: NodeKind,
}

pub enum NodeKind {
    Dir {
        children: Vec<u32>,
    },
    File {
        size: u64,
        mtime: i64,
        source: Source,
    },
}

pub enum Source {
    /// Entry index in the version's zip.
    Zip(usize),
    /// The downloaded upload itself.
    Raw,
    Inline(Bytes),
}

/// Node 0 is the version directory. Node numbering covers hidden nodes too, so
/// it depends only on the archive and stays stable forever.
pub struct Tree {
    pub nodes: Vec<Node>,
    by_name: HashMap<(u32, String), u32>,
}

impl Tree {
    /// Build the tree for the next version of a pack. `seen` must hold exactly
    /// the earlier versions, and is updated to include this one.
    pub fn build(manifest: &Manifest, version_name: &str, seen: &mut Seen) -> Result<Tree> {
        let mut t = Tree {
            nodes: Vec::new(),
            by_name: HashMap::new(),
        };
        t.nodes.push(Node {
            name: String::new(),
            parent: 0,
            visible: true,
            kind: NodeKind::Dir {
                children: Vec::new(),
            },
        });
        let mut dedup = String::from(
            "# Files of this version hidden because an identical file exists in an earlier version.\n# path-in-this-version\tearliest-copy (relative to the pack directory)\n",
        );
        let mut fresh: Vec<(DedupKey, String)> = Vec::new();

        'entries: for e in &manifest.entries {
            let mut dir = 0u32;
            let mut parts = e.path.split('/').peekable();
            while let Some(part) = parts.next() {
                let last = parts.peek().is_none();
                let existing = t.by_name.get(&(dir, part.to_string())).copied();
                if !last {
                    dir = match existing {
                        Some(i) if matches!(t.nodes[i as usize].kind, NodeKind::Dir { .. }) => i,
                        Some(_) => continue 'entries, // a file is in the way
                        None => t.add(
                            dir,
                            part,
                            NodeKind::Dir {
                                children: Vec::new(),
                            },
                        )?,
                    };
                    continue;
                }
                if existing.is_some() {
                    continue 'entries; // duplicate path in the archive
                }
                let source = match manifest.kind {
                    Kind::Zip => Source::Zip(e.index),
                    Kind::Raw => Source::Raw,
                };
                let node = t.add(
                    dir,
                    part,
                    NodeKind::File {
                        size: e.size,
                        mtime: e.mtime,
                        source,
                    },
                )?;
                let key = (part.to_string(), e.size, e.crc);
                match seen.0.get(&key) {
                    Some(earliest) => dedup.push_str(&format!("{}\t{}\n", e.path, earliest)),
                    None => {
                        t.show(node);
                        fresh.push((key, format!("{version_name}/{}", e.path)));
                    }
                }
            }
        }
        for (key, location) in fresh {
            seen.0.entry(key).or_insert(location);
        }
        if !t.by_name.contains_key(&(0, DEDUP_FILE.to_string())) {
            let source = Source::Inline(Bytes::from(dedup.clone()));
            let node = t.add(
                0,
                DEDUP_FILE,
                NodeKind::File {
                    size: dedup.len() as u64,
                    mtime: 0,
                    source,
                },
            )?;
            t.show(node);
        }
        Ok(t)
    }

    fn add(&mut self, parent: u32, name: &str, kind: NodeKind) -> Result<u32> {
        if self.nodes.len() >= MAX_NODES {
            bail!("archive has too many entries");
        }
        let i = self.nodes.len() as u32;
        self.nodes.push(Node {
            name: name.to_string(),
            parent,
            visible: false,
            kind,
        });
        if let NodeKind::Dir { children } = &mut self.nodes[parent as usize].kind {
            children.push(i);
        }
        self.by_name.insert((parent, name.to_string()), i);
        Ok(i)
    }

    fn show(&mut self, mut node: u32) {
        while !self.nodes[node as usize].visible {
            self.nodes[node as usize].visible = true;
            node = self.nodes[node as usize].parent;
        }
    }

    pub fn get(&self, node: u32) -> Option<&Node> {
        self.nodes.get(node as usize).filter(|n| n.visible)
    }

    pub fn child(&self, dir: u32, name: &str) -> Option<u32> {
        let i = *self.by_name.get(&(dir, name.to_string()))?;
        self.nodes[i as usize].visible.then_some(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Entry;

    fn manifest(files: &[(&str, u64, u32)]) -> Manifest {
        let entries = files
            .iter()
            .enumerate()
            .map(|(index, (p, size, crc))| Entry {
                path: p.to_string(),
                size: *size,
                crc: *crc,
                mtime: 0,
                index,
            })
            .collect();
        Manifest {
            kind: Kind::Zip,
            entries,
        }
    }

    fn visible_files(t: &Tree) -> Vec<String> {
        let mut out = Vec::new();
        for (i, n) in t.nodes.iter().enumerate() {
            if n.visible && matches!(n.kind, NodeKind::File { .. }) {
                let mut path = vec![n.name.clone()];
                let mut p = n.parent;
                while p != 0 {
                    path.push(t.nodes[p as usize].name.clone());
                    p = t.nodes[p as usize].parent;
                }
                path.reverse();
                out.push(path.join("/"));
                assert_eq!(t.child(n.parent, &n.name), Some(i as u32));
            }
        }
        out
    }

    #[test]
    fn later_versions_only_show_new_or_changed_files() {
        let mut seen = Seen::default();
        let v1 = Tree::build(
            &manifest(&[
                ("pack/a.png", 10, 1),
                ("pack/b.png", 20, 2),
                ("pack/copy/a.png", 10, 1),
            ]),
            "v1",
            &mut seen,
        )
        .unwrap();
        // Duplicates inside one version are all kept.
        assert_eq!(
            visible_files(&v1),
            ["pack/a.png", "pack/b.png", "pack/copy/a.png", DEDUP_FILE]
        );

        // v2: a unchanged (under a renamed top folder), b changed, c new.
        let v2 = Tree::build(
            &manifest(&[
                ("pack-v2/a.png", 10, 1),
                ("pack-v2/b.png", 21, 3),
                ("pack-v2/sub/c.png", 5, 4),
                ("old/b.png", 20, 2),
            ]),
            "v2",
            &mut seen,
        )
        .unwrap();
        assert_eq!(
            visible_files(&v2),
            ["pack-v2/b.png", "pack-v2/sub/c.png", DEDUP_FILE]
        );
        assert!(
            v2.child(0, "old").is_none(),
            "directories with nothing visible are hidden"
        );
        let NodeKind::File {
            source: Source::Inline(listing),
            ..
        } = &v2.nodes.last().unwrap().kind
        else {
            panic!()
        };
        let listing = std::str::from_utf8(listing).unwrap();
        assert!(listing.contains("pack-v2/a.png\tv1/pack/a.png\n"));
        assert!(listing.contains("old/b.png\tv1/pack/b.png\n"));

        // v3: b reverted to its v1 content -> hidden again, pointing back at v1.
        let v3 = Tree::build(
            &manifest(&[("pack/b.png", 20, 2), ("pack/sub/c.png", 5, 4)]),
            "v3",
            &mut seen,
        )
        .unwrap();
        assert_eq!(visible_files(&v3), [DEDUP_FILE]);
    }
}
