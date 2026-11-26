//! `prompt_append` of an `[agents.<name>]` table: the text of a file of the user after the agent's role prompt
//! (docs/design/config.md "Agent").

use super::{
    config::{fails, load_with},
    config_subagents::{FIT, ME, QA, base, me},
};
use crate::config::{EffectiveConfig, Layer, Name};

const SHOP: &str = "shop/loop-manager.toml";

fn implementer() -> Name {
    Name {
        pack: "kit".into(),
        name: "implementer".into(),
    }
}

/// BASE's project file with `prompt_append` of `[agents.<agent>]` naming `file`.
fn shop(agent: &str, file: &str) -> String {
    format!("{}[agents.{agent}]\nprompt_append = \"{file}\"\n", base(SHOP))
}

fn prompt(e: &EffectiveConfig) -> (&str, &Layer) {
    let prompt = e.system_prompt.as_ref().unwrap();
    (prompt.value.as_str(), &prompt.from)
}

#[test]
fn the_text_of_the_file_follows_the_role_prompt_after_a_blank_line() {
    let project = shop("implementer", "prompts/implementer.md");
    let rules = ("shop/prompts/implementer.md", "# Rules of this project\n\nUse tabs.\n");
    let config = load_with(&[(SHOP, &project), rules]).unwrap();
    let e = config.effective("shop", None, &implementer()).unwrap();
    let both = Layer::Merged(vec![Layer::Agent, Layer::ProjectAgent]);
    assert_eq!(
        prompt(&e),
        ("You implement.\n\n# Rules of this project\n\nUse tabs.", &both)
    );
    let text = e.explain();
    let lines = "system_prompt = \"You implement.\\n\\n# Rules of this project\\n\\nUse tabs.\"  # agent + project [agents]\n\
                 prompt_append = \"prompts/implementer.md\"  # project [agents]\n";
    assert!(text.contains(lines), "{text}");
    // A snapshot taken before the field reads as none.
    let mut old = serde_json::to_value(&e).unwrap();
    assert_eq!(old["prompt_append"][0]["value"], "prompts/implementer.md");
    old.as_object_mut().unwrap().remove("prompt_append");
    let old: EffectiveConfig = serde_json::from_value(old).unwrap();
    assert!(old.prompt_append.is_empty());

    // An addon's fragment comes after what the project appends.
    let mail = "name = \"mail\"\nversion = \"0.1.0\"\nprompt = \"prompt.md\"\n";
    let addon = [
        ("addons/mail/addon.toml", mail),
        ("addons/mail/prompt.md", "Check the mail.\n"),
    ];
    let config = load_with(&[(SHOP, &project), rules, addon[0], addon[1]]).unwrap();
    let e = config.effective("shop", None, &implementer()).unwrap();
    assert!(
        prompt(&e).0.ends_with("Use tabs.\n\nCheck the mail."),
        "{}",
        prompt(&e).0
    );

    // With no role prompt the text is the role.
    let project = shop("qa", "prompts/qa.md");
    let config = load_with(&[(SHOP, &project), (QA, FIT), ("shop/prompts/qa.md", "Check twice.\n")]).unwrap();
    let qa = Name {
        pack: "kit".into(),
        name: "qa".into(),
    };
    let e = config.effective("shop", None, &qa).unwrap();
    assert_eq!(prompt(&e), ("Check twice.", &Layer::ProjectAgent));
    // The file is named under the system prompt it joined, before the agent's other keys.
    let lines = "system_prompt = \"Check twice.\"  # project [agents]\n\
                 prompt_append = \"prompts/qa.md\"  # project [agents]\n\
                 description = ";
    assert!(e.explain().contains(lines), "{}", e.explain());
}

#[test]
fn the_orchestrator_the_project_and_the_rig_append_in_that_order() {
    let orchestrator = format!(
        "{}[agents.implementer]\nprompt_append = \"all.md\"\n",
        base("config.toml")
    );
    let on_rig = |keys: &str| {
        let rig = format!("model = \"opus\"\nprompt_append = \"backend/rules.md\"\n{keys}");
        shop("implementer", "prompts/implementer.md").replacen("model = \"opus\"\n", &rig, 1)
    };
    // Each file is relative to the directory of the config file that names it.
    let files = [
        ("all.md", "Everywhere.\n"),
        ("shop/prompts/implementer.md", "In the shop.\n"),
        ("shop/backend/rules.md", "On the backend.\n"),
    ];
    let load = |project: &str| {
        let mut changes = vec![("config.toml", orchestrator.as_str()), (SHOP, project)];
        changes.extend_from_slice(&files);
        load_with(&changes).unwrap()
    };
    let config = load(&on_rig(""));
    let e = config.effective("shop", Some("backend"), &implementer()).unwrap();
    let layers = [
        Layer::Agent,
        Layer::OrchestratorAgent,
        Layer::ProjectAgent,
        Layer::RigAgent,
    ];
    assert_eq!(
        prompt(&e),
        (
            "You implement.\n\nEverywhere.\n\nIn the shop.\n\nOn the backend.",
            &Layer::Merged(layers.to_vec())
        )
    );
    let files: Vec<&str> = e.prompt_append.iter().map(|s| s.value.as_str()).collect();
    assert_eq!(files, ["all.md", "prompts/implementer.md", "backend/rules.md"]);
    // The rig's file is for the runs on that rig.
    let e = config.effective("shop", None, &implementer()).unwrap();
    assert_eq!(prompt(&e).0, "You implement.\n\nEverywhere.\n\nIn the shop.");

    // An edit that sets `system_prompt` replaces the role; what the levels append still follows it.
    let config = load(&on_rig("system_prompt = \"You fix.\"\n"));
    let e = config.effective("shop", Some("backend"), &implementer()).unwrap();
    assert_eq!(
        prompt(&e).0,
        "You fix.\n\nEverywhere.\n\nIn the shop.\n\nOn the backend."
    );
}

#[test]
fn a_subagent_and_a_variant_get_what_is_appended_to_them() {
    let project = shop("qa", "prompts/qa.md");
    let config = load_with(&[
        (ME, &me("subagents = [\"qa\"]")),
        (QA, FIT),
        ("kit/agents/qa/prompt.md", "You check.\n"),
        (SHOP, &project),
        ("shop/prompts/qa.md", "Check twice.\n"),
    ])
    .unwrap();
    let (_, run) = config.for_run("shop", "backend", "kit.implement", "1").unwrap();
    assert_eq!(prompt(&run.targets["kit.qa"]).0, "You check.\n\nCheck twice.");
    // The caller's own role has nothing appended.
    assert_eq!(prompt(&run), ("You implement.", &Layer::Agent));

    // A variant's table takes the key as an edit does; an edit of the variant appends after it.
    let project = "packs = [\"../kit\"]\n\
        [agents.quick]\nbase = \"kit.implementer\"\nprompt_append = \"prompts/quick.md\"\n\
        [rigs.backend]\npath = \"backend\"\n\
        agents = { implementer = \"quick\", quick = { prompt_append = \"prompts/rig.md\" } }\n";
    let files = [
        ("shop/prompts/quick.md", "Be quick.\n"),
        ("shop/prompts/rig.md", "Here too.\n"),
    ];
    let config = load_with(&[(SHOP, project), files[0], files[1]]).unwrap();
    let (agent, run) = config.for_run("shop", "backend", "kit.implement", "1").unwrap();
    let layers = [Layer::Agent, Layer::Variant("quick".into()), Layer::RigAgent];
    assert_eq!(agent, "quick");
    assert_eq!(
        prompt(&run),
        (
            "You implement.\n\nBe quick.\n\nHere too.",
            &Layer::Merged(layers.to_vec())
        )
    );
}

#[test]
fn a_missing_or_empty_file_is_an_error_that_names_the_key_and_the_file() {
    let project = shop("implementer", "prompts/implementer.md");
    let errors = load_with(&[(SHOP, &project)]).unwrap_err();
    let error = (errors.iter())
        .find(|e| e.message.starts_with("`agents.implementer`: `prompt_append`: `"))
        .unwrap_or_else(|| panic!("{errors:?}"));
    assert!(
        error.message.contains("shop/prompts/implementer.md`: "),
        "{}",
        error.message
    );
    assert!(error.file.ends_with(SHOP));
    for empty in ["", " \n\t\n"] {
        fails(
            &[(SHOP, &project), ("shop/prompts/implementer.md", empty)],
            "shop/prompts/implementer.md` is empty",
        );
    }
    let rig = base(SHOP).replacen("model = \"opus\"\n", "prompt_append = \"none.md\"\n", 1);
    fails(
        &[(SHOP, &rig)],
        "rig `backend`: `agents.implementer`: `prompt_append`: `",
    );
    let number = base(SHOP).replacen("model = \"opus\"\n", "prompt_append = 1\n", 1);
    fails(&[(SHOP, &number)], "`prompt_append` must be a string");
    // The key is of the user's `[agents.<name>]` tables: an agent's own file is its `prompt`.
    fails(
        &[
            (ME, &me("prompt_append = \"more.md\"")),
            ("kit/agents/implementer/more.md", "More."),
        ],
        "unknown field `prompt_append`",
    );
}
