//! The addons a pack declares in `pack.toml` (docs/design/config.md "Pack"): declared for every project that lists
//! the pack, replaced by a declaration of the project or the orchestrator, one from two packs only from one source.

use super::config::{BASE, fails, load_with};
use crate::config::{Config, Name};

const NOTES: &str = "name = \"notes\"\nversion = \"0.1.0\"\n";
const PINNED: &str = "name = \"notes\"\nversion = \"0.2.0\"\n";
const MORE: &str = "name = \"more\"\nversion = \"0.1.0\"\n[[addon]]\n";

fn base(path: &str) -> &'static str {
    BASE.iter().find(|(p, _)| *p == path).unwrap().1
}

/// `kit/pack.toml` of `BASE` that requires the addon `notes` and declares what `decls` says.
fn kit(decls: &str) -> String {
    let pack = base("kit/pack.toml").replace("[\"mail\"]", "[\"mail\", \"notes\"]");
    format!("{pack}{decls}")
}

/// `files` with `more` over them.
fn over<'a>(files: &[(&'a str, &'a str)], more: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    [files, more].concat()
}

fn load(changed: &[(&str, &str)]) -> Config {
    load_with(changed).unwrap()
}

/// The name, the declaring pack and the version of each addon of the project.
fn declared(config: &Config) -> Vec<(&str, Option<&str>, &str)> {
    let addons = config.projects[0].addons.iter();
    addons
        .map(|a| (a.name.as_str(), a.pack.as_deref(), a.version.as_str()))
        .collect()
}

#[test]
fn a_pack_declares_addons_for_the_projects_that_list_it() {
    let pack = kit("[[addon]]\npath = \"addons/notes\"\n");
    let agent = base("kit/agents/implementer/agent.toml").replace("}]", "}, { name = \"notes\" }]");
    // The project attaches it too, with no declaration of its own.
    let project = format!(
        "{}[settings]\naddons = [{{ name = \"notes\" }}]\n",
        base("shop/loop-manager.toml")
    );
    let config = load(&[
        ("kit/pack.toml", &pack),
        ("kit/addons/notes/addon.toml", NOTES),
        ("kit/agents/implementer/agent.toml", &agent),
        ("shop/loop-manager.toml", &project),
    ]);
    assert_eq!(declared(&config), [("notes", Some("kit"), "0.1.0")]);
    let p = &config.projects[0];
    assert_eq!(p.addons[0].root, p.packs[1].root.join("addons/notes"), "from the pack");
    assert_eq!(config.addons[0].pack, None, "the orchestrator's own");
    let agent = Name {
        pack: "kit".into(),
        name: "implementer".into(),
    };
    let effective = config.effective("shop", None, &agent).unwrap();
    let attached: Vec<&str> = effective.addons.iter().map(|a| a.value.name.as_str()).collect();
    assert_eq!(attached, ["notes", "mail"]);
    // A pack that requires an addon nobody declares still fails.
    fails(&[("kit/pack.toml", &kit(""))], "required addon `notes` is not declared");
}

#[test]
fn a_declaration_of_the_project_or_the_orchestrator_replaces_a_packs() {
    let pack = kit("[[addon]]\npath = \"addons/notes\"\n");
    // The pack's own addon does not load; replaced, only its name is taken from it.
    let broken = format!("{NOTES}[[hook]]\non = \"no.such\"\ncommand = [\"true\"]\n");
    let files = [
        ("kit/pack.toml", pack.as_str()),
        ("kit/addons/notes/addon.toml", broken.as_str()),
        ("pinned/addon.toml", PINNED),
    ];
    fails(&files, "hook on unknown event type `no.such`");

    let project = format!("{}[[addon]]\npath = \"../pinned\"\n", base("shop/loop-manager.toml"));
    let config = load(&over(&files, &[("shop/loop-manager.toml", &project)]));
    assert_eq!(declared(&config), [("notes", None, "0.2.0")]);

    let top = format!("{}[[addon]]\npath = \"pinned\"\n", base("config.toml"));
    let config = load(&over(&files, &[("config.toml", &top)]));
    assert_eq!(declared(&config), []);
    assert_eq!(
        (config.addons[1].name.as_str(), config.addons[1].pack.as_deref()),
        ("notes", None)
    );
}

#[test]
fn two_packs_declare_one_addon_only_from_one_source() {
    let pack = kit("[[addon]]\npath = \"addons/notes\"\n");
    let project = base("shop/loop-manager.toml").replace("[\"../kit\"]", "[\"../kit\", \"../more\"]");
    let same = format!("{MORE}path = \"../kit/addons/notes\"\n");
    let other = format!("{MORE}path = \"notes\"\n");
    let files = [
        ("kit/pack.toml", pack.as_str()),
        ("kit/addons/notes/addon.toml", NOTES),
        ("more/notes/addon.toml", PINNED),
        ("shop/loop-manager.toml", project.as_str()),
    ];
    let config = load(&over(&files, &[("more/pack.toml", &same)]));
    assert_eq!(declared(&config), [("notes", Some("kit"), "0.1.0")], "one declaration");

    let errors = load_with(&over(&files, &[("more/pack.toml", &other)])).expect_err("two sources");
    let [error] = errors.as_slice() else {
        panic!("one error: {errors:?}");
    };
    assert!(error.file.ends_with("shop/loop-manager.toml"), "{error}");
    assert_eq!(
        error.message,
        "packs `kit` and `more` declare addon `notes` from different sources: declare it in this file with `[[addon]]`"
    );
    // The project's own declaration settles it.
    let settled = format!("{project}[[addon]]\npath = \"../more/notes\"\n");
    let config = load(&over(
        &files,
        &[("more/pack.toml", &other), ("shop/loop-manager.toml", &settled)],
    ));
    assert_eq!(declared(&config), [("notes", None, "0.2.0")]);

    let twice = kit("[[addon]]\npath = \"addons/notes\"\n[[addon]]\npath = \"../more/notes\"\n");
    fails(
        &over(&files[..3], &[("kit/pack.toml", &twice)]),
        "addon `notes` is declared twice",
    );
}
