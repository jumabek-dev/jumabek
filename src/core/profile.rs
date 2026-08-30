use std::path::PathBuf;

use crate::configs;
use crate::error::JumabekResult;
use crate::memory::facts::Fact;

pub const NOTES_LIMIT: usize = 8_000;

pub fn notes_path() -> Option<PathBuf> {
    configs::jumabek_dir().map(|dir| dir.join("profile.md"))
}

pub fn read_notes() -> String {
    let Some(path) = notes_path() else {
        return String::new();
    };

    let Ok(text) = std::fs::read_to_string(&path) else {
        return String::new();
    };

    let text = text.trim();
    match text.char_indices().nth(NOTES_LIMIT) {
        Some((idx, _)) => format!(
            "{}\n[profile.md is longer than this and was cut; tidy it up]",
            &text[..idx]
        ),
        None => text.to_string(),
    }
}

pub fn append_note(note: &str) -> JumabekResult<()> {
    let note = note.trim();
    if note.is_empty() {
        return Ok(());
    }

    let Some(path) = notes_path() else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim_start_matches("- ").trim() == note)
    {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("- {}\n", note));

    std::fs::write(path, updated)?;
    Ok(())
}

pub fn fetched_block(facts: &[Fact]) -> String {
    let rendered = crate::memory::facts::render(facts);
    if rendered.is_empty() {
        return String::new();
    }

    format!(
        "ALSO WORTH KNOWING, GIVEN WHAT IS BEING DISCUSSED\n\n         These were picked out because they look relevant to this turn, so the list changes \
         as the subject does. Use them the same way as the rest.\n\n{}\n",
        rendered
    )
}

pub fn block(facts: &[Fact], notes: &str) -> String {
    let rendered = crate::memory::facts::render(facts);

    if rendered.is_empty() && notes.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "WHAT YOU KNOW ABOUT THE PERSON YOU WORK FOR\n\n\
         These are things you were told and chose to keep. Treat a name in here as the \
         person it belongs to: if one is written down as also being called something else, \
         both names mean the same person. Do not read these aloud back to the user as a \
         list; use them the way someone who already knew would.\n",
    );

    if !rendered.is_empty() {
        out.push('\n');
        out.push_str(&rendered);
        out.push('\n');
    }

    if !notes.is_empty() {
        out.push_str("\nNotes:\n");
        out.push_str(notes);
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(subject: &str, key: &str, value: &str) -> Fact {
        Fact::new(subject, key, value)
    }

    #[test]
    fn nothing_known_produces_no_block() {
        assert!(block(&[], "").is_empty());
    }

    #[test]
    fn facts_and_notes_both_appear() {
        let facts = vec![fact("олжас", "alias", "балык")];
        let out = block(&facts, "- prefers short answers");

        assert!(out.contains("олжас"), "{out}");
        assert!(out.contains("балык"), "{out}");
        assert!(out.contains("prefers short answers"), "{out}");
    }

    #[test]
    fn notes_alone_are_enough_for_a_block() {
        let out = block(&[], "- lives in Almaty");
        assert!(out.contains("Almaty"), "{out}");
    }

    #[test]
    fn the_block_says_aliases_mean_the_same_person() {
        let out = block(&[fact("олжас", "alias", "олжик")], "");
        assert!(
            out.contains("both names mean the same person"),
            "the instruction that makes aliases usable is missing: {out}"
        );
    }
}
