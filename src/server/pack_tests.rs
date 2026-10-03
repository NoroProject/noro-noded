//! Что пак меняет на сервере, а что оставляет. Скачивание не проверяется —
//! для него нужен мастер; проверяется перенос, а он и решает, уцелеет ли мир.

use super::*;
use std::fs;

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("noded-pack-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".noro")).unwrap();
        Self { root }
    }

    /// A pack unpacked into staging, file by file.
    fn pack(&self, files: &[(&str, &str)]) -> PathBuf {
        let staging = self.root.join(".noro/pack-staging");
        let _ = fs::remove_dir_all(&staging);
        for (path, body) in files {
            let path = staging.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        staging
    }

    fn apply(&self, files: &[(&str, &str)]) -> bool {
        let staging = self.pack(files);
        apply(&self.root, &staging).unwrap()
    }

    fn put(&self, path: &str, body: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(path)).ok()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const GTNH_284: &[(&str, &str)] = &[
    ("mods/gregtech-5.09.51.jar", "gt-51"),
    ("mods/removed-later.jar", "x"),
    ("config/gregtech.cfg", "v284"),
    ("serverutilities/server/ranks.txt", "default ranks"),
    ("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "forge"),
    ("lwjgl3ify-forgePatches.jar", "lwjgl3ify 2.1.16"),
    ("old-launcher.jar", "dropped in 2.8.5"),
    ("server.properties", "motd=GTNH\nserver-port=25565\n"),
    ("eula.txt", "eula=false\n"),
    ("ops.json", "[]"),
];

const GTNH_285: &[(&str, &str)] = &[
    ("mods/gregtech-5.09.52.jar", "gt-52"),
    ("config/gregtech.cfg", "v285"),
    ("serverutilities/server/ranks.txt", "new default ranks"),
    ("serverutilities/server/new.txt", "added in 2.8.5"),
    ("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "forge"),
    ("lwjgl3ify-forgePatches.jar", "lwjgl3ify 2.1.17"),
    ("server.properties", "motd=GTNH\nserver-port=25565\n"),
    ("eula.txt", "eula=false\n"),
    ("ops.json", "[]"),
];

#[test]
fn a_first_install_takes_the_whole_pack() {
    let s = Sandbox::new();
    s.apply(GTNH_284);
    assert_eq!(
        s.read("mods/gregtech-5.09.51.jar").as_deref(),
        Some("gt-51")
    );
    assert_eq!(
        s.read("server.properties").as_deref(),
        Some("motd=GTNH\nserver-port=25565\n")
    );
    assert!(s.read("old-launcher.jar").is_some());
}

#[test]
fn an_update_keeps_what_the_server_made_of_itself() {
    let s = Sandbox::new();
    s.apply(GTNH_284);
    // What the server and its owner did since.
    fs::create_dir_all(s.root.join("world/region")).unwrap();
    fs::write(s.root.join("world/region/r.0.0.mca"), "chunks").unwrap();
    fs::write(s.root.join("server.properties"), "server-port=25601\n").unwrap();
    fs::write(s.root.join("ops.json"), r#"[{"name":"Dalynkaa"}]"#).unwrap();
    fs::write(
        s.root.join("serverutilities/server/ranks.txt"),
        "edited ranks",
    )
    .unwrap();

    s.apply(GTNH_285);

    assert_eq!(s.read("world/region/r.0.0.mca").as_deref(), Some("chunks"));
    assert_eq!(
        s.read("server.properties").as_deref(),
        Some("server-port=25601\n")
    );
    assert_eq!(
        s.read("ops.json").as_deref(),
        Some(r#"[{"name":"Dalynkaa"}]"#)
    );
    assert_eq!(
        s.read("serverutilities/server/ranks.txt").as_deref(),
        Some("edited ranks")
    );
    assert_eq!(
        s.read("serverutilities/server/new.txt").as_deref(),
        Some("added in 2.8.5")
    );
}

#[test]
fn an_update_replaces_the_packs_content() {
    let s = Sandbox::new();
    s.apply(GTNH_284);
    s.apply(GTNH_285);

    assert!(s.read("mods/gregtech-5.09.51.jar").is_none());
    assert!(s.read("mods/removed-later.jar").is_none());
    assert_eq!(
        s.read("mods/gregtech-5.09.52.jar").as_deref(),
        Some("gt-52")
    );
    assert_eq!(s.read("config/gregtech.cfg").as_deref(), Some("v285"));
    assert_eq!(
        s.read("lwjgl3ify-forgePatches.jar").as_deref(),
        Some("lwjgl3ify 2.1.17")
    );
    // The old pack brought it to the root, the new one doesn't.
    assert!(s.read("old-launcher.jar").is_none());
}

/// What the pack never brought is not the pack's to remove: the agent, the
/// chat and tab mods, mods installed through the panel and their configs.
#[test]
fn an_update_leaves_what_the_pack_did_not_bring() {
    let s = Sandbox::new();
    s.apply(GTNH_284);
    s.put("mods/noro-agent.jar", "agent");
    s.put("mods/noro-chat.jar", "chat");
    s.put("mods/noro-tab.jar", "tab");
    s.put("mods/hand-picked.jar", "from the panel");
    s.put("config/hand-picked.cfg", "its settings");
    s.put("config/noro-chat/chat.toml", "chat settings");

    s.apply(GTNH_285);

    for kept in [
        "mods/noro-agent.jar",
        "mods/noro-chat.jar",
        "mods/noro-tab.jar",
        "mods/hand-picked.jar",
        "config/hand-picked.cfg",
        "config/noro-chat/chat.toml",
    ] {
        assert!(s.read(kept).is_some(), "{kept} went missing");
    }
}

/// A server laid down before the node kept a list: there is no telling the
/// old pack's mods from anyone else's, so nothing is removed — and it says so.
#[test]
fn without_a_list_nothing_is_removed() {
    let s = Sandbox::new();
    s.put("mods/gregtech-5.09.51.jar", "gt-51");
    s.put("mods/hand-picked.jar", "from the panel");

    assert!(s.apply(GTNH_285));
    assert!(s.read("mods/gregtech-5.09.51.jar").is_some());
    assert!(s.read("mods/hand-picked.jar").is_some());
    // From here on the node has a list.
    assert!(!s.apply(GTNH_285));
}

#[test]
fn a_first_install_is_not_untracked() {
    let s = Sandbox::new();
    assert!(!s.apply(GTNH_284));
}

#[test]
fn a_renamed_world_is_kept_too() {
    let s = Sandbox::new();
    fs::write(s.root.join("server.properties"), "level-name=gtnh\n").unwrap();
    fs::create_dir_all(s.root.join("gtnh")).unwrap();
    fs::write(s.root.join("gtnh/level.dat"), "mine").unwrap();
    s.apply(&[("gtnh/level.dat", "the pack's")]);
    assert_eq!(s.read("gtnh/level.dat").as_deref(), Some("mine"));
}

#[test]
fn a_pack_wrapped_in_a_folder_is_unwrapped() {
    let s = Sandbox::new();
    s.apply(&[
        ("GTNH-Server/mods/a.jar", "a"),
        ("GTNH-Server/eula.txt", "eula=false\n"),
    ]);
    assert_eq!(s.read("mods/a.jar").as_deref(), Some("a"));
    assert!(s.read("GTNH-Server").is_none());
}
