use std::path::{Path, PathBuf};

pub const RELEASE: &str = include_str!("../../prompt.md");

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    InSync,
    Updated,
    LocalEdits,
    NeedsMerge { base: PathBuf },
    BaselineRecorded,
    Unreadable(String),
}

impl Status {
    pub fn note(&self) -> Option<String> {
        match self {
            Status::InSync | Status::LocalEdits | Status::BaselineRecorded => None,
            Status::Updated => Some(format!("prompt.md updated to {}", VERSION)),
            Status::NeedsMerge { base } => Some(format!(
                "prompt.md is older than this build ({}) and has your edits in it — \
                 the new one is at {}, and anything it describes that yours does not \
                 is invisible to the model",
                VERSION,
                base.display()
            )),
            Status::Unreadable(detail) => Some(format!("cannot check prompt.md: {}", detail)),
        }
    }
}

pub fn base_path(prompt: &Path) -> PathBuf {
    let mut name = prompt
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "prompt.md".to_string());
    name.push_str(".release");

    prompt.with_file_name(name)
}

pub fn reconcile(prompt: &Path) -> Status {
    reconcile_against(prompt, RELEASE)
}

fn reconcile_against(prompt: &Path, release: &str) -> Status {
    let current = match std::fs::read_to_string(prompt) {
        Ok(text) => text,
        Err(e) => return Status::Unreadable(format!("{}: {}", prompt.display(), e)),
    };

    let base = base_path(prompt);

    if current == release {
        let _ = std::fs::write(&base, release);
        return Status::InSync;
    }

    let recorded = match std::fs::read_to_string(&base) {
        Ok(text) => text,
        Err(_) => {
            let _ = std::fs::write(&base, release);
            return Status::BaselineRecorded;
        }
    };

    if recorded == release {
        return Status::LocalEdits;
    }

    if recorded == current {
        if let Err(e) = std::fs::write(prompt, release) {
            return Status::Unreadable(format!("cannot update {}: {}", prompt.display(), e));
        }
        let _ = std::fs::write(&base, release);
        return Status::Updated;
    }

    let _ = std::fs::write(&base, release);
    Status::NeedsMerge { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Sandbox {
            let dir = std::env::temp_dir().join(format!(
                "jumabek-prompt-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("sandbox");
            Sandbox { dir }
        }

        fn prompt(&self) -> PathBuf {
            self.dir.join("prompt.md")
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.dir.join(name), contents).expect("write");
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.join(name)).expect("read")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn an_untouched_prompt_is_brought_forward_without_asking() {
        let sandbox = Sandbox::new("untouched");
        sandbox.write("prompt.md", "old release");
        sandbox.write("prompt.md.release", "old release");

        assert_eq!(
            reconcile_against(&sandbox.prompt(), "new release"),
            Status::Updated
        );
        assert_eq!(sandbox.read("prompt.md"), "new release");
        assert_eq!(sandbox.read("prompt.md.release"), "new release");
    }

    #[test]
    fn an_edited_prompt_is_never_overwritten() {
        let sandbox = Sandbox::new("edited");
        sandbox.write("prompt.md", "old release plus my own rules");
        sandbox.write("prompt.md.release", "old release");

        let status = reconcile_against(&sandbox.prompt(), "new release");

        assert!(matches!(status, Status::NeedsMerge { .. }), "{status:?}");
        assert_eq!(
            sandbox.read("prompt.md"),
            "old release plus my own rules",
            "somebody's edits were thrown away"
        );
        assert!(status.note().is_some(), "the drift was not reported");
    }

    #[test]
    fn edits_alone_are_not_worth_a_word() {
        let sandbox = Sandbox::new("local");
        sandbox.write("prompt.md", "the release plus my own rules");
        sandbox.write("prompt.md.release", "the release");

        let status = reconcile_against(&sandbox.prompt(), "the release");

        assert_eq!(status, Status::LocalEdits);
        assert_eq!(status.note(), None, "nagging about a file nobody broke");
    }

    #[test]
    fn a_prompt_that_matches_the_build_says_nothing_and_records_itself() {
        let sandbox = Sandbox::new("insync");
        sandbox.write("prompt.md", "the release");

        assert_eq!(
            reconcile_against(&sandbox.prompt(), "the release"),
            Status::InSync
        );
        assert_eq!(sandbox.read("prompt.md.release"), "the release");
    }

    #[test]
    fn the_first_run_records_a_baseline_and_claims_nothing() {
        let sandbox = Sandbox::new("baseline");
        sandbox.write("prompt.md", "something older, possibly edited");

        let status = reconcile_against(&sandbox.prompt(), "the release");

        assert_eq!(status, Status::BaselineRecorded);
        assert_eq!(
            sandbox.read("prompt.md"),
            "something older, possibly edited",
            "a prompt of unknown provenance was overwritten"
        );
        assert_eq!(sandbox.read("prompt.md.release"), "the release");
    }

    #[test]
    fn a_missing_prompt_is_reported_rather_than_created() {
        let sandbox = Sandbox::new("missing");

        let status = reconcile_against(&sandbox.prompt(), "the release");

        assert!(matches!(status, Status::Unreadable(_)), "{status:?}");
        assert!(!sandbox.prompt().exists());
    }

    #[test]
    fn the_baseline_sits_beside_the_prompt_whatever_it_is_called() {
        assert_eq!(
            base_path(Path::new("/home/a/.jumabek/prompt.md")),
            PathBuf::from("/home/a/.jumabek/prompt.md.release")
        );
        assert_eq!(
            base_path(Path::new("custom-prompt.txt")),
            PathBuf::from("custom-prompt.txt.release")
        );
    }

    #[test]
    #[ignore]
    fn how_big_is_the_prompt() {
        let text = match std::env::var("COUNT_PROMPT") {
            Ok(path) => std::fs::read_to_string(path).expect("cannot read it"),
            Err(_) => RELEASE.to_string(),
        };
        println!(
            "{} chars, {} tokens",
            text.len(),
            crate::token_counter::count_message("system", &text)
        );
    }

    #[test]
    fn every_action_the_prompt_shows_still_parses() {
        let mut examples: Vec<String> = Vec::new();
        let mut building: Option<String> = None;

        for line in RELEASE.lines().map(str::trim) {
            match &mut building {
                Some(so_far) => {
                    so_far.push(' ');
                    so_far.push_str(line);
                }
                None if line.starts_with(r#"{"type":"#) => building = Some(line.to_string()),
                None => continue,
            }

            let open = building.as_deref().unwrap_or_default();
            if open.matches('{').count() == open.matches('}').count() {
                examples.push(building.take().unwrap_or_default());
            }
        }

        assert!(
            examples.len() > 15,
            "only found {} examples; the scan is broken",
            examples.len()
        );

        for example in &examples {
            serde_json::from_str::<crate::core::task::ActionType>(example).unwrap_or_else(|e| {
                panic!("the prompt shows an action the code cannot read:\n  {example}\n  {e}")
            });
        }
    }

    #[test]
    fn the_prompt_names_every_action_the_code_accepts() {
        for capability in crate::core::agent::CAPABILITIES {
            assert!(
                RELEASE.contains(capability),
                "{capability} is offered to the model and never explained"
            );
        }
    }

    #[test]
    fn the_shipped_prompt_is_the_real_one() {
        assert!(
            RELEASE.contains("You are JumaBek"),
            "the wrong file is embedded"
        );
        assert!(
            RELEASE.contains("execute_command"),
            "the shipped prompt does not use the shell skill's real method name"
        );
        assert!(
            !RELEASE.contains("\"method\":\"run\""),
            "the shipped prompt still documents a method that does not exist"
        );
    }
}
