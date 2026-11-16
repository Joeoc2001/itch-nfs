//! The directory tree of one pack: every version merged into one directory,
//! with cross-version de-duplication.
//!
//! Versions are folded in, in version number order. For each file of a
//! version:
//!
//! 1. identical (file name, size, CRC-32) to a file of an earlier version:
//!    not served;
//! 2. otherwise, if its path is free: served at its path;
//! 3. otherwise (an earlier version has other content there): served as
//!    `<stem>.v<version no><type suffix>` in the same directory.
//!
//! The tree only ever grows, and nothing in it is renamed, so a path that
//! works keeps working when a version is added.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, bail};
use bytes::Bytes;

use crate::archive::{Kind, Manifest};
use crate::ingest::archive_suffix;

/// Virtual file at the top of every pack, listing every file that is not
/// served at its own path, and where it (or its earliest copy) is.
pub const DEDUP_FILE: &str = ".itch-nfs-deduplicated.tsv";

const DEDUP_HEADER: &str = "# Files of this pack that are not served at the path they have in their upload.\n\
# renamed: the path was taken by an earlier version with other content.\n\
# duplicate: not served, because an identical file exists in an earlier version.\n\
# version\tpath-in-upload\tserved-path (relative to the pack directory)\twhy\n";

/// Nodes per version (28 bits of the file id).
pub const MAX_NODES: usize = 0xFFF_FFFF;

/// A node of a pack's tree: the version that brought it, and its number in
/// that version. The number is the position in the version's own tree (node 0
/// being the version's top), counting hidden nodes too, so it depends only on
/// that version's listing in the archive and stays stable forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId {
    pub ver: u16,
    pub node: u32,
}

/// The pack directory. Version numbers start at 1, so version 0 is free.
pub const ROOT: NodeId = NodeId { ver: 0, node: 0 };
/// [`DEDUP_FILE`].
pub const LISTING: NodeId = NodeId { ver: 0, node: 1 };

/// An asset's identity across versions: file name, size and CRC-32. The
/// directory is deliberately not part of it, so that assets survive a pack
/// being re-zipped under a different top-level folder.
type DedupKey = (String, u64, u32);

pub struct Node {
    /// The name it is served under, which for a clash is the suffixed one.
    pub name: String,
    pub parent: NodeId,
    /// Files: the entry's own, or when the upload was published. Directories:
    /// when the last version that added an entry to it was archived, and
    /// never going backwards, so that clients re-read a directory that grew.
    pub mtime: i64,
    pub kind: NodeKind,
}

pub enum NodeKind {
    Dir {
        /// In the order they were added. Only ever appended to.
        children: Vec<NodeId>,
    },
    File {
        size: u64,
        crc: u32,
        source: Source,
    },
}

#[derive(Clone)]
pub enum Source {
    /// Entry index in the zip of the version the node belongs to.
    Zip(usize),
    /// The downloaded upload itself.
    Raw,
    Inline(Bytes),
}

/// What the tree needs to know about a version.
pub struct Version<'a> {
    pub no: u16,
    pub name: &'a str,
    /// When the upload was published.
    pub timestamp: i64,
    /// When the version was archived.
    pub archived_at: i64,
}

/// `name` with a version tag before its type suffix: `tile.png` ->
/// `tile.v2.png`, `sfx.tar.gz` -> `sfx.v2.tar.gz`. Names with no type suffix,
/// and dotfiles, get it appended: `LICENSE.v2`, `.gitignore.v2`. `nth` above 1
/// is for when that name is taken too: `tile.v2-2.png`.
pub fn suffixed(name: &str, version: u16, nth: u32) -> String {
    let cut = match archive_suffix(name) {
        // Archive suffixes are ASCII, so this is a character boundary.
        Some(s) if s.len() < name.len() => name.len() - s.len(),
        _ => match name.rfind('.') {
            Some(dot) if dot > 0 => dot,
            _ => name.len(),
        },
    };
    let (stem, kind) = name.split_at(cut);
    if nth > 1 {
        format!("{stem}.v{version}-{nth}{kind}")
    } else {
        format!("{stem}.v{version}{kind}")
    }
}

/// A node of one version's own tree, as it is in the upload.
struct Local {
    name: String,
    parent: u32,
    /// False for files that duplicate an earlier version, and for directories
    /// left with nothing visible in them.
    visible: bool,
    /// Directories: the children. Files: none.
    children: Vec<u32>,
    /// Files: the index of the manifest entry.
    entry: Option<usize>,
}

/// One version's own tree. Node 0 is its top.
struct LocalTree {
    nodes: Vec<Local>,
    by_name: HashMap<(u32, String), u32>,
}

impl LocalTree {
    fn add(&mut self, parent: u32, name: &str, entry: Option<usize>) -> Result<u32> {
        if self.nodes.len() >= MAX_NODES {
            bail!("archive has too many entries");
        }
        let i = self.nodes.len() as u32;
        self.nodes.push(Local {
            name: name.to_string(),
            parent,
            visible: false,
            children: Vec::new(),
            entry,
        });
        self.nodes[parent as usize].children.push(i);
        self.by_name.insert((parent, name.to_string()), i);
        Ok(i)
    }

    fn show(&mut self, mut node: u32) {
        while !self.nodes[node as usize].visible {
            self.nodes[node as usize].visible = true;
            node = self.nodes[node as usize].parent;
        }
    }
}

/// Every version of a pack folded in so far, as one tree.
pub struct PackTree {
    nodes: HashMap<NodeId, Node>,
    /// Served names only.
    by_name: HashMap<(NodeId, String), NodeId>,
    /// Every asset in the versions folded in -> where its earliest copy is
    /// served.
    seen: HashMap<DedupKey, String>,
    /// The rows of [`DEDUP_FILE`].
    rows: String,
    folded: BTreeSet<u16>,
}

impl Default for PackTree {
    fn default() -> Self {
        let mut t = PackTree {
            nodes: HashMap::new(),
            by_name: HashMap::new(),
            seen: HashMap::new(),
            rows: String::new(),
            folded: BTreeSet::new(),
        };
        t.nodes.insert(
            ROOT,
            Node {
                name: String::new(),
                parent: ROOT,
                mtime: 0,
                kind: NodeKind::Dir {
                    children: Vec::new(),
                },
            },
        );
        t.set_listing();
        t
    }
}

impl PackTree {
    /// The numbers of the versions folded in.
    pub fn folded(&self) -> &BTreeSet<u16> {
        &self.folded
    }

    /// Fold in the next version of the pack, which must have a higher number
    /// than every version folded in so far. On an error nothing has changed.
    pub fn extend(&mut self, v: &Version, manifest: &Manifest) -> Result<()> {
        if v.no == 0 || self.folded.last().is_some_and(|&last| v.no <= last) {
            bail!("version {} ({}) folded in out of order", v.no, v.name);
        }

        // The version's own tree, numbered exactly as it would be on its own.
        let mut local = LocalTree {
            nodes: vec![Local {
                name: String::new(),
                parent: 0,
                visible: true,
                children: Vec::new(),
                entry: None,
            }],
            by_name: HashMap::new(),
        };
        // (file, key) of the files that are new, (entry, earliest copy) of
        // the ones that are not.
        let mut fresh: Vec<(u32, DedupKey)> = Vec::new();
        let mut hidden: Vec<(usize, String)> = Vec::new();
        'entries: for (at, e) in manifest.entries.iter().enumerate() {
            let mut dir = 0u32;
            let mut parts = e.path.split('/').peekable();
            while let Some(part) = parts.next() {
                let last = parts.peek().is_none();
                let existing = local.by_name.get(&(dir, part.to_string())).copied();
                if !last {
                    dir = match existing {
                        Some(i) if local.nodes[i as usize].entry.is_none() => i,
                        Some(_) => continue 'entries, // a file is in the way
                        None => local.add(dir, part, None)?,
                    };
                    continue;
                }
                if existing.is_some() {
                    continue 'entries; // duplicate path in the archive
                }
                let node = local.add(dir, part, Some(at))?;
                let key = (part.to_string(), e.size, e.crc);
                // Only earlier versions count: duplicates inside one version
                // are all kept.
                match self.seen.get(&key) {
                    Some(earliest) => hidden.push((at, earliest.clone())),
                    None => {
                        local.show(node);
                        fresh.push((node, key));
                    }
                }
            }
        }

        // Merge what is visible into the pack's tree, directory by directory.
        // In each, the entries whose name is free are placed first and the
        // ones that need a suffix after them (in name order), so that the
        // names do not depend on the order of the entries in the archive.
        let mut created: HashSet<NodeId> = HashSet::new();
        let mut work = vec![(0u32, ROOT)];
        while let Some((from, into)) = work.pop() {
            let children = std::mem::take(&mut local.nodes[from as usize].children);
            let mut clashing = Vec::new();
            for c in children {
                let child = &local.nodes[c as usize];
                if !child.visible {
                    continue;
                }
                match self.by_name.get(&(into, child.name.clone())).copied() {
                    None => {
                        let name = child.name.clone();
                        self.place(v, manifest, &local, c, into, name, &mut created, &mut work);
                    }
                    // Directories are shared between versions.
                    Some(shared)
                        if child.entry.is_none()
                            && matches!(self.nodes[&shared].kind, NodeKind::Dir { .. }) =>
                    {
                        work.push((c, shared));
                    }
                    Some(_) => clashing.push(c),
                }
            }
            clashing.sort_by(|a, b| {
                local.nodes[*a as usize]
                    .name
                    .cmp(&local.nodes[*b as usize].name)
            });
            for c in clashing {
                let mut nth = 1;
                let name = loop {
                    let name = suffixed(&local.nodes[c as usize].name, v.no, nth);
                    if !self.by_name.contains_key(&(into, name.clone())) {
                        break name;
                    }
                    nth += 1;
                };
                self.place(v, manifest, &local, c, into, name, &mut created, &mut work);
            }
        }

        for (node, key) in fresh {
            let e = &manifest.entries[local.nodes[node as usize].entry.unwrap()];
            let served = self.path(NodeId { ver: v.no, node });
            if served != e.path {
                self.rows
                    .push_str(&format!("{}\t{}\t{served}\trenamed\n", v.name, e.path));
            }
            self.seen.entry(key).or_insert(served);
        }
        for (at, earliest) in hidden {
            self.rows.push_str(&format!(
                "{}\t{}\t{earliest}\tduplicate\n",
                v.name, manifest.entries[at].path
            ));
        }
        self.folded.insert(v.no);
        self.set_listing();
        Ok(())
    }

    /// Add node `c` of the version being folded in to directory `into`, as
    /// `name`.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        v: &Version,
        manifest: &Manifest,
        local: &LocalTree,
        c: u32,
        into: NodeId,
        name: String,
        created: &mut HashSet<NodeId>,
        work: &mut Vec<(u32, NodeId)>,
    ) {
        let id = NodeId { ver: v.no, node: c };
        let (kind, mtime) = match local.nodes[c as usize].entry {
            None => {
                created.insert(id);
                work.push((c, id));
                (
                    NodeKind::Dir {
                        children: Vec::new(),
                    },
                    v.archived_at,
                )
            }
            Some(at) => {
                let e = &manifest.entries[at];
                (
                    NodeKind::File {
                        size: e.size,
                        crc: e.crc,
                        source: match manifest.kind {
                            Kind::Zip => Source::Zip(e.index),
                            Kind::Raw => Source::Raw,
                        },
                    },
                    if e.mtime > 0 { e.mtime } else { v.timestamp },
                )
            }
        };
        self.by_name.insert((into, name.clone()), id);
        self.nodes.insert(
            id,
            Node {
                name,
                parent: into,
                mtime,
                kind,
            },
        );
        let parent = self.nodes.get_mut(&into).unwrap();
        if let NodeKind::Dir { children } = &mut parent.kind {
            children.push(id);
        }
        // Once per version; a directory this version made is already stamped.
        if created.insert(into) {
            parent.mtime = v.archived_at.max(parent.mtime + 1);
        }
    }

    fn set_listing(&mut self) {
        let text = format!("{DEDUP_HEADER}{}", self.rows);
        self.nodes.insert(
            LISTING,
            Node {
                name: DEDUP_FILE.to_string(),
                parent: ROOT,
                mtime: 0,
                kind: NodeKind::File {
                    size: text.len() as u64,
                    crc: 0,
                    source: Source::Inline(Bytes::from(text)),
                },
            },
        );
    }

    /// A real file or directory of that name at the top takes precedence
    /// over the listing.
    fn listing_shadowed(&self) -> bool {
        self.by_name.contains_key(&(ROOT, DEDUP_FILE.to_string()))
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        if id == LISTING && self.listing_shadowed() {
            return None;
        }
        self.nodes.get(&id)
    }

    pub fn child(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        match self.by_name.get(&(dir, name.to_string())) {
            Some(id) => Some(*id),
            None => (dir == ROOT && name == DEDUP_FILE).then_some(LISTING),
        }
    }

    /// The entries of a directory, in a stable order: entries are only ever
    /// added at the end. `None` if `dir` is not a directory.
    pub fn children(&self, dir: NodeId) -> Option<Vec<NodeId>> {
        let NodeKind::Dir { children } = &self.nodes.get(&dir)?.kind else {
            return None;
        };
        let mut out = Vec::with_capacity(children.len() + 1);
        // First, so that it does not move as the directory grows.
        if dir == ROOT && !self.listing_shadowed() {
            out.push(LISTING);
        }
        out.extend(children);
        Some(out)
    }

    /// The served path of a node, relative to the pack directory.
    pub fn path(&self, mut id: NodeId) -> String {
        let mut parts = Vec::new();
        while id != ROOT {
            let n = &self.nodes[&id];
            parts.push(n.name.as_str());
            id = n.parent;
        }
        parts.reverse();
        parts.join("/")
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

    /// Fold in version `no`, archived at second `100 * no`.
    fn fold(t: &mut PackTree, no: u16, files: &[(&str, u64, u32)]) {
        let name = format!("v{no}");
        let v = Version {
            no,
            name: &name,
            timestamp: 10 * no as i64,
            archived_at: 100 * no as i64,
        };
        t.extend(&v, &manifest(files)).unwrap();
    }

    fn tree(versions: &[&[(&str, u64, u32)]]) -> PackTree {
        let mut t = PackTree::default();
        for (i, files) in versions.iter().enumerate() {
            fold(&mut t, i as u16 + 1, files);
        }
        t
    }

    fn walk(t: &PackTree, dir: NodeId, prefix: &str, out: &mut Vec<(String, NodeId)>) {
        for c in t.children(dir).unwrap() {
            let n = t.get(c).unwrap();
            assert_eq!(t.child(dir, &n.name), Some(c));
            assert_eq!(n.parent, dir);
            let path = format!("{prefix}{}", n.name);
            assert_eq!(t.path(c), path);
            match n.kind {
                NodeKind::Dir { .. } => walk(t, c, &format!("{path}/"), out),
                NodeKind::File { .. } => out.push((path, c)),
            }
        }
    }

    /// Every served file but the listing, sorted by path, with its id.
    fn served(t: &PackTree) -> Vec<(String, NodeId)> {
        let mut out = Vec::new();
        walk(t, ROOT, "", &mut out);
        assert_eq!(out.remove(0), (DEDUP_FILE.to_string(), LISTING));
        out.sort();
        out
    }

    fn files(t: &PackTree) -> Vec<String> {
        served(t).into_iter().map(|(p, _)| p).collect()
    }

    /// The rows of the listing, without the header.
    fn rows(t: &PackTree) -> Vec<String> {
        let NodeKind::File {
            source: Source::Inline(text),
            size,
            ..
        } = &t.get(LISTING).unwrap().kind
        else {
            panic!()
        };
        assert_eq!(*size, text.len() as u64);
        std::str::from_utf8(text)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    /// (size, crc) of the file served at `path`.
    fn content(t: &PackTree, path: &str) -> (u64, u32) {
        let mut id = ROOT;
        for part in path.split('/') {
            id = t.child(id, part).unwrap_or_else(|| panic!("no {path}"));
        }
        match t.get(id).unwrap().kind {
            NodeKind::File { size, crc, .. } => (size, crc),
            NodeKind::Dir { .. } => panic!("{path} is a directory"),
        }
    }

    #[test]
    fn version_tags_go_before_the_type_suffix() {
        for (name, tagged, again) in [
            ("tile.png", "tile.v2.png", "tile.v2-3.png"),
            ("sfx.tar.gz", "sfx.v2.tar.gz", "sfx.v2-3.tar.gz"),
            ("SFX.TAR.GZ", "SFX.v2.TAR.GZ", "SFX.v2-3.TAR.GZ"),
            ("a.b.tzst", "a.b.v2.tzst", "a.b.v2-3.tzst"),
            (
                "notes.final.txt",
                "notes.final.v2.txt",
                "notes.final.v2-3.txt",
            ),
            ("LICENSE", "LICENSE.v2", "LICENSE.v2-3"),
            (".gitignore", ".gitignore.v2", ".gitignore.v2-3"),
            (".zip", ".zip.v2", ".zip.v2-3"),
            (".config.json", ".config.v2.json", ".config.v2-3.json"),
            ("grüße.tar.xz", "grüße.v2.tar.xz", "grüße.v2-3.tar.xz"),
            ("日本.語", "日本.v2.語", "日本.v2-3.語"),
        ] {
            assert_eq!(suffixed(name, 2, 1), tagged);
            assert_eq!(suffixed(name, 2, 3), again);
        }
        assert_eq!(suffixed("tile.png", 65535, 1), "tile.v65535.png");
    }

    #[test]
    fn versions_are_merged_into_one_directory() {
        let mut t = tree(&[&[
            ("pack/a.png", 10, 1),
            ("pack/b.png", 20, 2),
            ("pack/copy/a.png", 10, 1),
        ]]);
        // Duplicates inside one version are all kept.
        assert_eq!(files(&t), ["pack/a.png", "pack/b.png", "pack/copy/a.png"]);
        assert_eq!(rows(&t), Vec::<String>::new());

        // v2: a unchanged, b changed, c new, and an unchanged b elsewhere.
        fold(
            &mut t,
            2,
            &[
                ("pack/a.png", 10, 1),
                ("pack/b.png", 21, 3),
                ("pack/sub/c.png", 5, 4),
                ("old/b.png", 20, 2),
            ],
        );
        assert_eq!(
            files(&t),
            [
                "pack/a.png",
                "pack/b.png",
                "pack/b.v2.png",
                "pack/copy/a.png",
                "pack/sub/c.png"
            ]
        );
        // The plain path keeps the oldest content; the newer one is tagged.
        assert_eq!(content(&t, "pack/b.png"), (20, 2));
        assert_eq!(content(&t, "pack/b.v2.png"), (21, 3));
        assert!(
            t.child(ROOT, "old").is_none(),
            "directories with nothing visible are hidden"
        );
        assert_eq!(
            rows(&t),
            [
                "v2\tpack/b.png\tpack/b.v2.png\trenamed",
                "v2\tpack/a.png\tpack/a.png\tduplicate",
                "v2\told/b.png\tpack/b.png\tduplicate",
            ]
        );

        // v3: b reverted to its v1 content -> hidden again; b's v2 content
        // under another name -> hidden, pointing at the tagged copy.
        fold(
            &mut t,
            3,
            &[
                ("pack/b.png", 20, 2),
                ("x/b.png", 21, 3),
                ("pack/sub/c.png", 5, 4),
            ],
        );
        assert_eq!(files(&t).len(), 5, "nothing new is served");
        assert_eq!(
            rows(&t)[3..],
            [
                "v3\tpack/b.png\tpack/b.png\tduplicate",
                "v3\tx/b.png\tpack/b.v2.png\tduplicate",
                "v3\tpack/sub/c.png\tpack/sub/c.png\tduplicate",
            ]
        );

        // v4: a third content for b.
        fold(&mut t, 4, &[("pack/b.png", 22, 5)]);
        assert_eq!(content(&t, "pack/b.v4.png"), (22, 5));
        assert_eq!(content(&t, "pack/b.v2.png"), (21, 3));
        assert_eq!(content(&t, "pack/b.png"), (20, 2));
        assert_eq!(t.folded().iter().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
    }

    #[test]
    fn a_pack_rezipped_under_another_folder_only_adds_what_changed() {
        let t = tree(&[
            &[("pack/a.png", 10, 1), ("pack/b.png", 20, 2)],
            &[("pack-v2/a.png", 10, 1), ("pack-v2/b.png", 21, 3)],
        ]);
        // Another path, so no clash and no tag.
        assert_eq!(files(&t), ["pack-v2/b.png", "pack/a.png", "pack/b.png"]);
        assert_eq!(rows(&t), ["v2\tpack-v2/a.png\tpack/a.png\tduplicate"]);
    }

    #[test]
    fn every_kind_of_name_gets_its_tag() {
        let t = tree(&[
            &[
                ("sfx.tar.gz", 1, 1),
                ("LICENSE", 1, 2),
                (".gitignore", 1, 3),
                ("d/tile.png", 1, 4),
            ],
            &[
                ("sfx.tar.gz", 2, 11),
                ("LICENSE", 2, 12),
                (".gitignore", 2, 13),
                ("d/tile.png", 2, 14),
            ],
        ]);
        assert_eq!(
            files(&t),
            [
                ".gitignore",
                ".gitignore.v2",
                "LICENSE",
                "LICENSE.v2",
                "d/tile.png",
                "d/tile.v2.png",
                "sfx.tar.gz",
                "sfx.v2.tar.gz",
            ]
        );
    }

    #[test]
    fn a_file_and_a_directory_of_one_name_clash_too() {
        // v1: x.zip could not be opened (a file), y.zip could (a directory).
        // v2: the other way round. v3: both directories again.
        let t = tree(&[
            &[("x.zip", 1, 1), ("y.zip/in.txt", 1, 2)],
            &[
                ("x.zip/a.txt", 2, 3),
                ("x.zip/deep/b.txt", 2, 4),
                ("y.zip", 2, 5),
            ],
            &[("x.zip/c.txt", 3, 6), ("y.zip/more.txt", 3, 7)],
        ]);
        assert_eq!(
            files(&t),
            [
                "x.v2.zip/a.txt",
                "x.v2.zip/deep/b.txt",
                "x.v3.zip/c.txt",
                "x.zip",
                "y.v2.zip",
                "y.zip/in.txt",
                "y.zip/more.txt",
            ]
        );
        assert_eq!(
            rows(&t),
            [
                "v2\tx.zip/a.txt\tx.v2.zip/a.txt\trenamed",
                "v2\tx.zip/deep/b.txt\tx.v2.zip/deep/b.txt\trenamed",
                "v2\ty.zip\ty.v2.zip\trenamed",
                "v3\tx.zip/c.txt\tx.v3.zip/c.txt\trenamed",
            ]
        );
    }

    #[test]
    fn a_taken_tagged_name_gets_a_counter() {
        // The upload of version 2 really contains tile.v2.png, and v3 has a
        // tile.v2.png of its own.
        let t = tree(&[
            &[("tile.png", 1, 1)],
            &[("tile.v2.png", 2, 2), ("tile.png", 3, 3)],
            &[("tile.v2.png", 4, 4), ("tile.v2.v3.png", 5, 5)],
        ]);
        assert_eq!(
            files(&t),
            [
                "tile.png",
                "tile.v2-2.png",
                "tile.v2.png",
                "tile.v2.v3-2.png",
                "tile.v2.v3.png",
            ]
        );
        assert_eq!(content(&t, "tile.v2.png"), (2, 2));
        assert_eq!(content(&t, "tile.v2-2.png"), (3, 3));
        assert_eq!(content(&t, "tile.v2.v3.png"), (5, 5));
        assert_eq!(content(&t, "tile.v2.v3-2.png"), (4, 4));
    }

    #[test]
    fn names_do_not_depend_on_the_order_of_the_entries() {
        let v1: &[(&str, u64, u32)] = &[("d/tile.png", 1, 1), ("x.zip", 1, 2), ("n", 1, 3)];
        let v2 = [
            ("d/tile.png", 2, 11),
            ("d/tile.v2.png", 2, 12),
            ("x.zip/a.txt", 2, 13),
            ("x.v2.zip/b.txt", 2, 14),
            ("n", 2, 15),
            ("n.v2", 2, 16),
            ("d/new.png", 2, 17),
        ];
        let contents = |t: &PackTree| -> Vec<(String, (u64, u32))> {
            files(t)
                .into_iter()
                .map(|p| {
                    let c = content(t, &p);
                    (p, c)
                })
                .collect()
        };
        let expected = contents(&tree(&[v1, &v2]));
        assert_eq!(
            expected.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            [
                "d/new.png",
                "d/tile.png",
                "d/tile.v2-2.png",
                "d/tile.v2.png",
                "n",
                "n.v2",
                "n.v2-2",
                "x.v2-2.zip/a.txt",
                "x.v2.zip/b.txt",
                "x.zip",
            ]
        );
        assert_eq!(expected[2].1, (2, 11));
        assert_eq!(expected[3].1, (2, 12));
        // Every rotation and the reverse of version 2's entries.
        let mut orders: Vec<Vec<(&str, u64, u32)>> = (0..v2.len())
            .map(|by| {
                let mut o = v2.to_vec();
                o.rotate_left(by);
                o
            })
            .collect();
        orders.push(v2.iter().rev().copied().collect());
        for order in orders {
            assert_eq!(contents(&tree(&[v1, &order])), expected, "{order:?}");
        }
    }

    #[test]
    fn ids_come_from_each_version_alone_and_never_change() {
        let v1: &[(&str, u64, u32)] = &[("pack/a.png", 10, 1), ("pack/sub/b.png", 20, 2)];
        let v2: &[(&str, u64, u32)] = &[
            ("pack/a.png", 10, 1),     // hidden, but numbered
            ("pack/sub/b.png", 21, 3), // tagged
            ("pack/sub/c.png", 5, 4),
            ("other/d.png", 6, 5),
        ];
        let id = |ver, node| NodeId { ver, node };
        let mut t = tree(&[v1]);
        // Numbered as the version's own tree is: 0 its top, then each
        // directory and file in the order the listing first names them.
        let before = served(&t);
        assert_eq!(
            before,
            [
                ("pack/a.png".to_string(), id(1, 2)),
                ("pack/sub/b.png".to_string(), id(1, 4)),
            ]
        );
        assert_eq!(t.child(ROOT, "pack"), Some(id(1, 1)));
        assert_eq!(t.child(id(1, 1), "sub"), Some(id(1, 3)));
        let listed = t.children(id(1, 3)).unwrap();

        fold(&mut t, 2, v2);
        let after = served(&t);
        for f in &before {
            assert!(after.contains(f), "{f:?} moved");
        }
        assert_eq!(
            after,
            [
                ("other/d.png".to_string(), id(2, 7)),
                ("pack/a.png".to_string(), id(1, 2)),
                ("pack/sub/b.png".to_string(), id(1, 4)),
                ("pack/sub/b.v2.png".to_string(), id(2, 4)),
                ("pack/sub/c.png".to_string(), id(2, 5)),
            ]
        );
        // A merged directory keeps the id the first version gave it, and a
        // directory only grows at its end.
        assert_eq!(t.child(ROOT, "pack"), Some(id(1, 1)));
        assert_eq!(t.child(id(1, 1), "sub"), Some(id(1, 3)));
        assert_eq!(t.child(ROOT, "other"), Some(id(2, 6)));
        assert_eq!(
            t.children(id(1, 3)).unwrap(),
            [listed.as_slice(), &[id(2, 5), id(2, 4)]].concat()
        );
        // Version 2's hidden file, and the directories it shares, have no
        // node of their own.
        for hidden in [id(2, 1), id(2, 2), id(2, 3), id(2, 0), id(1, 0)] {
            assert!(t.get(hidden).is_none(), "{hidden:?}");
        }

        // Without version 1, version 2's nodes have the same numbers.
        let alone = tree(&[&[], v2]);
        assert_eq!(
            served(&alone),
            [
                ("other/d.png".to_string(), id(2, 7)),
                ("pack/a.png".to_string(), id(2, 2)),
                ("pack/sub/b.png".to_string(), id(2, 4)),
                ("pack/sub/c.png".to_string(), id(2, 5)),
            ]
        );
    }

    #[test]
    fn a_directory_changes_when_a_version_adds_to_it() {
        let mtime = |t: &PackTree, path: &str| {
            let mut id = ROOT;
            for part in path.split('/') {
                id = t.child(id, part).unwrap();
            }
            t.get(id).unwrap().mtime
        };
        let mut t = tree(&[&[("pack/a.png", 1, 1), ("pack/sub/b.png", 1, 2)]]);
        assert_eq!(mtime(&t, "pack"), 100);
        assert_eq!(mtime(&t, "pack/sub"), 100);
        // A file with no time of its own has the time of its upload.
        assert_eq!(mtime(&t, "pack/a.png"), 10);

        // Version 2 adds to pack/sub only, twice.
        fold(
            &mut t,
            2,
            &[("pack/sub/c.png", 2, 3), ("pack/sub/b.png", 2, 4)],
        );
        assert_eq!(mtime(&t, "pack"), 100, "nothing was added to it");
        assert_eq!(mtime(&t, "pack/sub"), 200);
        assert_eq!(mtime(&t, "pack/a.png"), 10);

        // A version archived no later than the one before still moves it on.
        let v = Version {
            no: 3,
            name: "v3",
            timestamp: 0,
            archived_at: 150,
        };
        t.extend(
            &v,
            &manifest(&[("pack/sub/d.png", 3, 5), ("pack/new/e.png", 3, 6)]),
        )
        .unwrap();
        assert_eq!(mtime(&t, "pack/sub"), 201);
        assert_eq!(mtime(&t, "pack"), 150);
        assert_eq!(mtime(&t, "pack/new"), 150);

        // A version with nothing new changes nothing.
        fold(&mut t, 4, &[("pack/sub/d.png", 3, 5)]);
        assert_eq!(mtime(&t, "pack/sub"), 201);
    }

    #[test]
    fn a_real_file_takes_precedence_over_the_listing() {
        let mut t = tree(&[&[("a.png", 1, 1)]]);
        assert_eq!(t.child(ROOT, DEDUP_FILE), Some(LISTING));
        assert_eq!(t.children(ROOT).unwrap()[0], LISTING);
        fold(&mut t, 2, &[(DEDUP_FILE, 7, 7)]);
        let real = NodeId { ver: 2, node: 1 };
        assert_eq!(t.child(ROOT, DEDUP_FILE), Some(real));
        assert!(t.get(LISTING).is_none());
        assert_eq!(
            t.children(ROOT).unwrap(),
            [NodeId { ver: 1, node: 1 }, real]
        );
    }

    #[test]
    fn paths_that_cannot_exist_are_skipped_within_a_version() {
        let t = tree(&[&[
            ("a", 1, 1),
            ("a/b", 1, 2), // a file is in the way
            ("a", 1, 3),   // the path repeats
            ("c", 1, 4),
        ]]);
        assert_eq!(files(&t), ["a", "c"]);
        assert_eq!(content(&t, "a"), (1, 1));
    }

    #[test]
    fn versions_must_be_folded_in_order() {
        let mut t = tree(&[&[("a", 1, 1)], &[("b", 1, 2)]]);
        let m = manifest(&[("c", 1, 3)]);
        for no in [0, 1, 2] {
            let v = Version {
                no,
                name: "again",
                timestamp: 0,
                archived_at: 0,
            };
            assert!(t.extend(&v, &m).is_err());
        }
        assert_eq!(files(&t), ["a", "b"], "a refused version changes nothing");
        // Gaps are fine.
        fold(&mut t, 7, &[("a", 2, 9)]);
        assert_eq!(files(&t), ["a", "a.v7", "b"]);
    }
}
