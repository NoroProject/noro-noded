//! Один каталог снимка — чтобы выбрать, что из него вернуть.
//!
//! Каталоги выводятся из путей файлов, а не только из записей-каталогов:
//! снимок, где каталог пропущен (например, правилом исключения), всё равно
//! должен открываться по своим файлам. Размер каталога — сумма файлов под ним:
//! по нему видно, где лежит мир, а где конфиги.

use schema::noded::{DirEntry, DirListing};
use std::collections::BTreeMap;

use super::manifest::Manifest;

pub fn list(manifest: &Manifest, dir: &str) -> DirListing {
    let dir = dir.trim_matches('/').trim_start_matches("./");
    let dir = if dir == "." { "" } else { dir };
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };

    let mut found: BTreeMap<&str, DirEntry> = BTreeMap::new();
    for entry in &manifest.entries {
        let Some(rest) = entry.path.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let (name, deeper) = match rest.split_once('/') {
            Some((name, _)) => (name, true),
            None => (rest, entry.dir),
        };
        let item = found.entry(name).or_insert_with(|| DirEntry {
            name: name.to_string(),
            path: format!("{prefix}{name}"),
            dir: deeper,
            size: 0,
            modified: None,
            symlink: false,
            mode: None,
        });
        item.dir |= deeper;
        item.size += entry.size;
        if !deeper {
            item.modified = Some(entry.mtime);
            item.mode = Some(entry.mode);
        }
    }

    // Каталоги первыми, как в файловом менеджере.
    let mut entries: Vec<DirEntry> = found.into_values().collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    DirListing {
        path: dir.to_string(),
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::manifest::{Entry, VERSION};

    fn entry(path: &str, dir: bool, size: u64) -> Entry {
        Entry {
            path: path.into(),
            dir,
            mode: 0o644,
            mtime: 1,
            size,
            chunks: vec![],
            stamp: None,
        }
    }

    #[test]
    fn a_directory_lists_its_children_with_their_sizes() {
        let m = Manifest {
            version: VERSION,
            backup: uuid::Uuid::nil(),
            created: 0,
            entries: vec![
                entry("server.properties", false, 10),
                entry("world", true, 0),
                entry("world/level.dat", false, 5),
                entry("world/region/r.0.0.mca", false, 100),
                // Каталога-записи нет, но файлы в нём есть.
                entry("plugins/x/config.yml", false, 7),
            ],
        };

        let root = list(&m, "");
        let names: Vec<_> = root
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.dir, e.size))
            .collect();
        assert_eq!(
            names,
            vec![
                ("plugins", true, 7),
                ("world", true, 105),
                ("server.properties", false, 10)
            ]
        );

        let world = list(&m, "/world/");
        assert_eq!(world.path, "world");
        let paths: Vec<_> = world.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["world/region", "world/level.dat"]);
    }
}
