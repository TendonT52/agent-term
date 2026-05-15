use std::process::ExitCode;

struct Skill {
    name: &'static str,
    content: &'static str,
    references: &'static [(&'static str, &'static str)],
}

const SKILLS: &[Skill] = &[Skill {
    name: "core",
    content: include_str!("../skill-data/core/SKILL.md"),
    references: &[
        (
            "references/scenarios.md",
            include_str!("../skill-data/core/references/scenarios.md"),
        ),
        (
            "references/commands.md",
            include_str!("../skill-data/core/references/commands.md"),
        ),
        (
            "references/json-schemas.md",
            include_str!("../skill-data/core/references/json-schemas.md"),
        ),
        (
            "references/troubleshooting.md",
            include_str!("../skill-data/core/references/troubleshooting.md"),
        ),
        (
            "references/safety.md",
            include_str!("../skill-data/core/references/safety.md"),
        ),
    ],
}];

fn find(name: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|s| s.name == name)
}

pub fn run_list() -> ExitCode {
    for s in SKILLS {
        println!("{}", s.name);
    }
    ExitCode::SUCCESS
}

pub fn run_get(name: &str, full: bool) -> ExitCode {
    let Some(skill) = find(name) else {
        eprintln!(
            "agent-term: skills: unknown skill {name:?}. Available: {}",
            SKILLS
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::from(1);
    };

    print!("{}", skill.content);
    if !skill.content.ends_with('\n') {
        println!();
    }

    if full {
        if skill.references.is_empty() {
            eprintln!(
                "agent-term: skills: --full requested but no reference files are bundled with this build of {name}"
            );
        } else {
            for (path, body) in skill.references {
                println!("\n---\n# {path}\n");
                print!("{body}");
                if !body.ends_with('\n') {
                    println!();
                }
            }
        }
    }

    ExitCode::SUCCESS
}
