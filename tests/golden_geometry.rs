//! Geometry checked against real `mke2fs` output.
//!
//! Each fixture in `tests/golden/` was produced by e2fsprogs 1.47.3 on a Linux
//! host with the UUID, hash seed and `SOURCE_DATE_EPOCH` pinned, which makes
//! `mke2fs` byte-reproducible. The `.dump` files are that filesystem's
//! `dumpe2fs` output.
//!
//! These tests read the reference's own numbers and require ours to match. When
//! one fails, the reference is right.

use std::collections::HashMap;

use mkfs_ext4::layout::Geometry;
use mkfs_ext4::params::{Params, Profile};

/// Parse the `key: value` header `dumpe2fs` prints before the group listing.
fn parse_dump(name: &str) -> HashMap<String, String> {
    let path = format!("{}/tests/golden/{name}.dump", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));

    let mut out = HashMap::new();
    for line in text.lines() {
        // The per-group listing starts at "Group 0: (Blocks ...)". Match the
        // digit too: the header itself contains "Group descriptor size".
        if line
            .strip_prefix("Group ")
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn num(dump: &HashMap<String, String>, key: &str) -> u64 {
    let raw = dump
        .get(key)
        .unwrap_or_else(|| panic!("golden dump has no '{key}' (keys: {:?})", dump.keys()));
    raw.split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|e| panic!("parsing '{key}' = '{raw}': {e}"))
}

struct Case {
    name: &'static str,
    size: u64,
    params: fn() -> Params,
}

const MIB: u64 = 1024 * 1024;

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "ext4-64m-default",
            size: 64 * MIB,
            params: || Params::new(Profile::Ext4),
        },
        Case {
            name: "ext4-64m-nojournal",
            size: 64 * MIB,
            params: || Params::new(Profile::Ext4).no_journal(),
        },
        Case {
            name: "ext2-64m",
            size: 64 * MIB,
            params: || Params::new(Profile::Ext2),
        },
        Case {
            name: "ext3-64m",
            size: 64 * MIB,
            params: || Params::new(Profile::Ext3),
        },
        Case {
            name: "ext4-16m-1k",
            size: 16 * MIB,
            params: || Params::new(Profile::Ext4).no_journal().block_size(1024),
        },
        Case {
            name: "ext4-256m",
            size: 256 * MIB,
            params: || Params::new(Profile::Ext4),
        },
    ]
}

/// Every number `mke2fs` chose, we must choose too.
#[test]
fn geometry_matches_every_golden_reference() {
    let mut failures = Vec::new();

    for case in cases() {
        let dump = parse_dump(case.name);
        let params = (case.params)();
        let g = match Geometry::compute(case.size, &params) {
            Ok(g) => g,
            Err(e) => {
                failures.push(format!("{}: geometry failed: {e}", case.name));
                continue;
            }
        };

        let mut check = |field: &str, ours: u64, theirs: u64| {
            if ours != theirs {
                failures.push(format!(
                    "{}: {field} — ours {ours}, mke2fs {theirs}",
                    case.name
                ));
            }
        };

        check("block size", g.block_size as u64, num(&dump, "Block size"));
        check("block count", g.blocks_count, num(&dump, "Block count"));
        check("inode count", g.inodes_count as u64, num(&dump, "Inode count"));
        check(
            "reserved block count",
            g.r_blocks_count,
            num(&dump, "Reserved block count"),
        );
        check("first block", g.first_data_block as u64, num(&dump, "First block"));
        check(
            "blocks per group",
            g.blocks_per_group as u64,
            num(&dump, "Blocks per group"),
        );
        check(
            "inodes per group",
            g.inodes_per_group as u64,
            num(&dump, "Inodes per group"),
        );
        check("inode size", g.inode_size as u64, num(&dump, "Inode size"));
        check(
            "inode blocks per group",
            g.itable_blocks_per_group as u64,
            num(&dump, "Inode blocks per group"),
        );
        check(
            "reserved GDT blocks",
            g.reserved_gdt_blocks as u64,
            num(&dump, "Reserved GDT blocks"),
        );

        // Present only on filesystems that carry them.
        if dump.contains_key("Group descriptor size") {
            check(
                "group descriptor size",
                g.desc_size as u64,
                num(&dump, "Group descriptor size"),
            );
        }
        if dump.contains_key("Flex block group size") {
            check(
                "flex block group size",
                g.flex_bg_size() as u64,
                num(&dump, "Flex block group size"),
            );
        }
    }

    assert!(
        failures.is_empty(),
        "geometry differs from real mke2fs:\n  {}",
        failures.join("\n  ")
    );
}

/// Features real `mke2fs` sets that this implementation does not write yet.
///
/// Listed rather than quietly tolerated, so the gap is visible and shrinks on
/// purpose. Empty: every feature `mke2fs` sets on these references, we set too.
const KNOWN_GAPS: &[&str] = &[];

/// The feature masks we resolve must be the ones `mke2fs` wrote.
#[test]
fn features_match_every_golden_reference() {
    let mut failures = Vec::new();

    for case in cases() {
        let dump = parse_dump(case.name);
        let theirs: std::collections::BTreeSet<&str> = dump
            .get("Filesystem features")
            .expect("golden dump has no feature list")
            .split_whitespace()
            .collect();

        let params = (case.params)();
        let masks = params.resolve_features().expect("features resolve");
        let ours: std::collections::BTreeSet<String> =
            masks.to_spec().split(',').map(str::to_string).collect();

        for feature in &theirs {
            if !ours.contains(*feature) && !KNOWN_GAPS.contains(feature) {
                failures.push(format!("{}: mke2fs set '{feature}', we do not", case.name));
            }
        }
        for feature in &ours {
            if !theirs.contains(feature.as_str()) {
                failures.push(format!("{}: we set '{feature}', mke2fs does not", case.name));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "feature sets differ from real mke2fs:\n  {}",
        failures.join("\n  ")
    );
}
