//! The docs and the spec table have to agree. `script::COMMANDS` is the
//! source of truth, so a new command that nobody wrote up fails here rather
//! than shipping undocumented.

use ttmux::script::{COMMANDS, EXIT_CODES};

fn api_md() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/API.md")).unwrap()
}

#[test]
fn api_md_documents_every_command_and_every_flag() {
    let doc = api_md();
    for c in COMMANDS {
        let heading = format!("### {}\n", c.name);
        assert!(
            doc.contains(&heading),
            "API.md has no section for {}",
            c.name
        );
        for f in c.flags {
            assert!(
                doc.contains(f.long),
                "API.md does not mention {} of {}",
                f.long,
                c.name
            );
        }
    }
}

#[test]
fn api_md_lists_the_exit_codes_the_code_returns() {
    let doc = api_md();
    for (code, about) in EXIT_CODES {
        assert!(
            doc.contains(&format!("| {code} | {about} |")),
            "API.md is missing exit code {code}"
        );
    }
}

#[test]
fn the_readme_points_at_the_reference() {
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    assert!(readme.contains("[API.md](API.md)"));
}
