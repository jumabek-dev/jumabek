pub fn extract_json_payload(content: &str) -> String {
    let without_reasoning = strip_reasoning(content.trim());
    let trimmed = strip_code_fence(without_reasoning.trim());

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }

    let Some(candidate) = extract_braced_substring(trimmed) else {
        return trimmed.to_string();
    };

    if serde_json::from_str::<serde_json::Value>(&candidate).is_ok() {
        return candidate;
    }

    escape_raw_control_chars_in_strings(&candidate)
}

/// Markdown is full of backslashes — `\|` inside a table, `\*` before a
/// literal asterisk — and a model writing markdown into a JSON string routinely
/// emits them unescaped, which makes the whole answer unreadable. Escape the
/// ones JSON does not recognise and leave every real escape alone.
pub fn repair_escapes(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len());
    let mut chars = payload.chars().peekable();
    let mut inside_string = false;

    while let Some(c) = chars.next() {
        if c == '"' {
            inside_string = !inside_string;
            out.push(c);
            continue;
        }

        if c != '\\' || !inside_string {
            out.push(c);
            continue;
        }

        match chars.peek().copied() {
            Some('u') => {
                let rest: String = chars.clone().skip(1).take(4).collect();
                if rest.len() == 4 && rest.chars().all(|h| h.is_ascii_hexdigit()) {
                    out.push('\\');
                } else {
                    out.push_str("\\\\");
                }
            }

            // A backslash that ends the payload cannot be a real escape.
            None => out.push_str("\\\\"),

            Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => out.push('\\'),

            Some(_) => out.push_str("\\\\"),
        }
    }

    out
}

pub fn looks_truncated(content: &str) -> bool {
    let without_reasoning = strip_reasoning(content.trim());
    let trimmed = strip_code_fence(without_reasoning.trim());
    let has_open = trimmed.contains('{') || trimmed.contains('[');
    has_open && extract_braced_substring(trimmed).is_none()
}

fn strip_reasoning(text: &str) -> &str {
    const BLOCKS: [(&str, &str); 3] = [
        ("<think>", "</think>"),
        ("<thinking>", "</thinking>"),
        ("<reasoning>", "</reasoning>"),
    ];

    let mut rest = text;

    loop {
        let mut moved = false;

        for (open, close) in BLOCKS {
            let Some(start) = rest.find(open) else {
                continue;
            };

            rest = match rest[start..].find(close) {
                Some(end) => &rest[start + end + close.len()..],
                None => "",
            }
            .trim_start();

            moved = true;
        }

        if !moved {
            return rest;
        }
    }
}

fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };

    let body = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => return text,
    };

    body.trim_end()
        .strip_suffix("```")
        .map(|s| s.trim())
        .unwrap_or(body)
}

fn extract_braced_substring(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|&c| c == '{' || c == '[')?;

    let open = chars[start];
    let close = if open == '{' { '}' } else { ']' };

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut end: Option<usize> = None;

    for (i, &c) in chars.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }

        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            c if !in_string && c == open => depth += 1,
            c if !in_string && c == close => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }

    end.map(|e| chars[start..=e].iter().collect())
}

fn escape_raw_control_chars_in_strings(json: &str) -> String {
    let mut result = String::with_capacity(json.len() + 16);
    let mut in_string = false;
    let mut escaped = false;

    for ch in json.chars() {
        if escaped {
            result.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_string => {
                escaped = true;
                result.push(ch);
            }
            '"' => {
                in_string = !in_string;
                result.push(ch);
            }
            '\n' if in_string => result.push_str("\\n"),
            '\r' if in_string => result.push_str("\\r"),
            '\t' if in_string => result.push_str("\\t"),
            c if in_string && (c as u32) < 0x20 => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            _ => result.push(ch),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backslash_markdown_uses_is_escaped_so_the_answer_can_be_read() {
        let broken = r#"{"message":"a \| b and \* c"}"#;
        assert!(serde_json::from_str::<serde_json::Value>(broken).is_err());

        let mended = repair_escapes(broken);
        let read: serde_json::Value = serde_json::from_str(&mended).expect("still unreadable");
        assert_eq!(read["message"], "a \\| b and \\* c");
    }

    #[test]
    fn every_escape_json_really_has_is_left_alone() {
        let fine = r#"{"message":"line\ntab\tquote\"slash\\ unicode\u0416"}"#;
        let read: serde_json::Value = serde_json::from_str(fine).expect("the fixture is wrong");

        assert_eq!(repair_escapes(fine), fine, "a valid escape was mangled");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&repair_escapes(fine)).unwrap(),
            read
        );
    }

    #[test]
    fn a_half_written_unicode_escape_is_treated_as_a_plain_backslash() {
        let broken = r#"{"message":"cost \u20 only"}"#;
        let read: serde_json::Value =
            serde_json::from_str(&repair_escapes(broken)).expect("still unreadable");
        assert_eq!(read["message"], "cost \\u20 only");
    }

    #[test]
    fn a_backslash_outside_a_string_is_not_touched() {
        let payload = r#"{"a":1} \ {"b":2}"#;
        assert_eq!(repair_escapes(payload), payload);
    }

    #[test]
    fn a_payload_ending_on_a_backslash_does_not_run_off_the_end() {
        assert_eq!(repair_escapes(r#"{"m":"x\"#), r#"{"m":"x\\"#);
    }

    fn parses(s: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(s).is_ok()
    }

    #[test]
    fn passes_clean_json_through() {
        let raw = r#"{"message":"ok","is_done":true,"actions":[]}"#;
        assert_eq!(extract_json_payload(raw), raw);
    }

    #[test]
    fn unwraps_markdown_fence() {
        let raw = "```json\n{\"message\":\"ok\",\"is_done\":true,\"actions\":[]}\n```";
        let out = extract_json_payload(raw);
        assert!(parses(&out), "got: {out}");
        assert!(out.starts_with('{'));
    }

    #[test]
    fn strips_prose_around_json() {
        let raw =
            "Sure! Here is the result:\n{\"message\":\"ok\",\"actions\":[]}\nHope that helps.";
        let out = extract_json_payload(raw);
        assert!(parses(&out), "got: {out}");
        assert!(!out.contains("Hope"));
    }

    #[test]
    fn escapes_raw_newlines_inside_strings() {
        let raw = "{\"message\":\"line one\nline two\",\"actions\":[]}";
        assert!(!parses(raw));
        let out = extract_json_payload(raw);
        assert!(parses(&out), "got: {out}");
    }

    #[test]
    fn keeps_braces_inside_strings() {
        let raw = r#"{"message":"use {} for empty","actions":[]}"#;
        let out = extract_json_payload(raw);
        assert!(parses(&out));
        assert!(out.contains("use {} for empty"));
    }

    #[test]
    fn detects_truncation() {
        assert!(looks_truncated(r#"{"message":"cut off here"#));
        assert!(!looks_truncated(r#"{"message":"fine"}"#));
        assert!(!looks_truncated("no json at all"));
    }

    #[test]
    fn survives_plain_text() {
        assert_eq!(extract_json_payload("just words"), "just words");
    }

    #[test]
    fn a_reasoning_block_full_of_braces_does_not_win_over_the_answer() {
        let content = "<think>\nI should reply with {\"message\": ...} and set is_done.\n\
                       Maybe {\"actions\": []} too.\n</think>\n\
                       {\"message\":\"hi\",\"is_done\":true,\"actions\":[]}";

        let payload = extract_json_payload(content);
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("not JSON");

        assert_eq!(parsed["message"], "hi");
        assert_eq!(parsed["is_done"], true);
    }

    #[test]
    fn a_thought_that_never_closes_is_not_mined_for_braces() {
        let content = "<think>I will answer with {\"message\": something";

        assert!(
            !extract_json_payload(content).contains("message"),
            "a half-written thought was read as the answer"
        );
    }

    #[test]
    fn several_thoughts_in_a_row_are_all_dropped() {
        let content = "<think>first {</think> noise <thinking>second {</thinking>\
                       {\"message\":\"done\",\"is_done\":true,\"actions\":[]}";

        let parsed: serde_json::Value =
            serde_json::from_str(&extract_json_payload(content)).expect("not JSON");
        assert_eq!(parsed["message"], "done");
    }

    #[test]
    fn a_reply_with_no_reasoning_is_untouched() {
        let plain = "{\"message\":\"hi\",\"is_done\":true,\"actions\":[]}";
        assert_eq!(extract_json_payload(plain), plain);
    }

    #[test]
    fn a_thought_before_a_fenced_answer_still_finds_it() {
        let content = "<think>planning</think>\n```json\n                       {\"message\":\"hi\",\"is_done\":true,\"actions\":[]}\n```";

        let parsed: serde_json::Value =
            serde_json::from_str(&extract_json_payload(content)).expect("not JSON");
        assert_eq!(parsed["message"], "hi");
    }
}
