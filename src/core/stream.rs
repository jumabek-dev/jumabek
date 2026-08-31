use crate::core::llm::Protocol;
use crate::core::usage::Usage;

pub struct Assembler {
    protocol: Protocol,
    pending: String,
    answer: String,
    usage: Option<Usage>,
    shown: usize,
    done: bool,
    events: usize,
}

impl Assembler {
    pub fn new(protocol: Protocol) -> Assembler {
        Assembler {
            protocol,
            pending: String::new(),
            answer: String::new(),
            usage: None,
            shown: 0,
            done: false,
            events: 0,
        }
    }

    pub fn feed(&mut self, bytes: &str) -> Option<String> {
        self.pending.push_str(bytes);

        while let Some(at) = self.pending.find('\n') {
            let line = self.pending[..at].trim_end_matches('\r').to_string();
            self.pending.drain(..=at);
            self.take(&line);
        }

        self.newly_visible()
    }

    pub fn finish(mut self) -> (String, Option<Usage>) {
        let leftover = std::mem::take(&mut self.pending);
        if !leftover.trim().is_empty() {
            self.take(leftover.trim_end_matches('\r'));
        }

        (self.answer, self.usage)
    }

    pub fn saw_the_end(&self) -> bool {
        self.done
    }

    /// Whether anything at all arrived as a server-sent event. False means the
    /// endpoint ignored the request to stream and sent a plain body instead.
    pub fn saw_any_events(&self) -> bool {
        self.events > 0
    }

    fn take(&mut self, line: &str) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };

        self.events += 1;

        let payload = payload.trim();
        if payload.is_empty() {
            return;
        }

        if payload == "[DONE]" {
            self.done = true;
            return;
        }

        let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };

        match self.protocol {
            Protocol::OpenAi => self.take_openai(&event),
            Protocol::Anthropic => self.take_anthropic(&event),
        }
    }

    fn take_openai(&mut self, event: &serde_json::Value) {
        if let Some(usage) = crate::core::usage::from_json(event.get("usage")) {
            self.usage = Some(usage);
        }

        let Some(delta) = event
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
        else {
            return;
        };

        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            self.answer.push_str(text);
        }

        if let Some(arguments) = delta
            .get("tool_calls")
            .and_then(|calls| calls.as_array())
            .and_then(|calls| calls.first())
            .and_then(|call| call.get("function"))
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
        {
            self.answer.push_str(arguments);
        }
    }

    fn take_anthropic(&mut self, event: &serde_json::Value) {
        match event.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(usage) =
                    crate::core::usage::from_json(event.get("message").and_then(|m| m.get("usage")))
                {
                    self.usage = Some(usage);
                }
            }

            Some("content_block_delta") => {
                let Some(delta) = event.get("delta") else {
                    return;
                };

                for field in ["text", "partial_json"] {
                    if let Some(text) = delta.get(field).and_then(|t| t.as_str()) {
                        self.answer.push_str(text);
                    }
                }
            }

            Some("message_delta") => {
                if let Some(output) = event
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|n| n.as_u64())
                    && let Some(usage) = self.usage.as_mut()
                {
                    usage.output = output as u32;
                }
            }

            Some("message_stop") => self.done = true,

            _ => {}
        }
    }

    fn newly_visible(&mut self) -> Option<String> {
        let whole = visible_message(&self.answer)?;

        if whole.len() <= self.shown {
            return None;
        }

        let fresh = whole[self.shown..].to_string();
        self.shown = whole.len();
        Some(fresh)
    }
}

pub fn visible_message(json_so_far: &str) -> Option<String> {
    let key = json_so_far.find("\"message\"")?;
    let after = &json_so_far[key + "\"message\"".len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let mut chars = rest.chars();

    if chars.next()? != '"' {
        return None;
    }

    let mut out = String::new();

    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => {}
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() < 4 {
                        return Some(out);
                    }
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(decoded) => out.push(decoded),
                        None => return Some(out),
                    }
                }
                Some(other) => out.push(other),
                None => return Some(out),
            },
            other => out.push(other),
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai(chunks: &[&str]) -> Assembler {
        let mut a = Assembler::new(Protocol::OpenAi);
        for chunk in chunks {
            a.feed(chunk);
        }
        a
    }

    #[test]
    fn an_openai_answer_is_rebuilt_from_its_pieces() {
        let a = openai(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"mess\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"age\\\":\\\"hi\\\"}\"}}]}\n",
            "data: [DONE]\n",
        ]);

        assert!(a.saw_the_end());
        let (answer, _) = a.finish();
        assert_eq!(answer, r#"{"message":"hi"}"#);
    }

    #[test]
    fn a_chunk_that_splits_a_line_in_half_is_still_read() {
        let mut a = Assembler::new(Protocol::OpenAi);
        a.feed("data: {\"choices\":[{\"delta\":{\"conte");
        a.feed("nt\":\"abc\"}}]}\n");

        let (answer, _) = a.finish();
        assert_eq!(answer, "abc");
    }

    #[test]
    fn a_last_line_with_no_newline_after_it_is_not_lost() {
        let mut a = Assembler::new(Protocol::OpenAi);
        a.feed("data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}");

        let (answer, _) = a.finish();
        assert_eq!(answer, "tail");
    }

    #[test]
    fn openai_usage_arrives_in_the_last_chunk_and_is_kept() {
        let a = openai(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":41,\"completion_tokens\":7}}\n",
        ]);

        let (_, usage) = a.finish();
        let usage = usage.expect("usage was thrown away");
        assert_eq!(usage.input, 41);
        assert_eq!(usage.output, 7);
    }

    #[test]
    fn a_model_answering_through_tool_calls_is_rebuilt_too() {
        let a = openai(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"1}\"}}]}}]}\n",
        ]);

        let (answer, _) = a.finish();
        assert_eq!(answer, r#"{"a":1}"#);
    }

    #[test]
    fn an_anthropic_tool_answer_is_rebuilt_from_partial_json() {
        let mut a = Assembler::new(Protocol::Anthropic);
        for line in [
            r#"event: content_block_delta"#,
            r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"message\":"}}"#,
            r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":" \"Hello ther"}}"#,
            r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"e!\"}"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ] {
            a.feed(line);
            a.feed("\n");
        }

        assert!(a.saw_the_end());
        let (answer, _) = a.finish();
        assert_eq!(answer, r#"{"message": "Hello there!"}"#);
    }

    #[test]
    fn anthropic_plain_text_deltas_are_rebuilt_when_no_tool_is_used() {
        let mut a = Assembler::new(Protocol::Anthropic);
        a.feed("data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"one \"}}\n");
        a.feed("data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"two\"}}\n");

        let (answer, _) = a.finish();
        assert_eq!(answer, "one two");
    }

    #[test]
    fn anthropic_counts_the_input_at_the_start_and_the_output_at_the_end() {
        let mut a = Assembler::new(Protocol::Anthropic);
        a.feed("data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":681,\"output_tokens\":1,\"cache_read_input_tokens\":42,\"cache_creation_input_tokens\":0}}}\n");
        a.feed("data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":213}}\n");

        let (_, usage) = a.finish();
        let usage = usage.expect("usage was thrown away");
        assert_eq!(usage.input, 681);
        assert_eq!(
            usage.output, 213,
            "the final output count was not picked up"
        );
        assert_eq!(usage.cache_read, Some(42));
    }

    #[test]
    fn a_ping_or_an_unknown_event_changes_nothing() {
        let mut a = Assembler::new(Protocol::Anthropic);
        a.feed("event: ping\ndata: {\"type\":\"ping\"}\n");
        a.feed(
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\"}}\n",
        );
        a.feed(": a comment\n\n");

        let (answer, usage) = a.finish();
        assert!(answer.is_empty());
        assert!(usage.is_none());
    }

    #[test]
    fn a_line_that_is_not_json_is_skipped_rather_than_killing_the_turn() {
        let mut a = Assembler::new(Protocol::OpenAi);
        a.feed("data: not json at all\n");
        a.feed("data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n");

        let (answer, _) = a.finish();
        assert_eq!(answer, "ok");
    }

    #[test]
    fn the_message_is_readable_while_it_is_still_being_written() {
        assert_eq!(
            visible_message(r#"{"message":"Прове"#).as_deref(),
            Some("Прове")
        );
        assert_eq!(
            visible_message(r#"{"message":"done","is_done":true}"#).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn nothing_is_shown_before_the_message_field_exists() {
        assert_eq!(visible_message(""), None);
        assert_eq!(visible_message(r#"{"is_done"#), None);
        assert_eq!(visible_message(r#"{"message""#), None);
        assert_eq!(visible_message(r#"{"message":"#), None);
    }

    #[test]
    fn escapes_inside_the_message_are_decoded_as_they_arrive() {
        assert_eq!(
            visible_message(r#"{"message":"one\ntwo"#).as_deref(),
            Some("one\ntwo")
        );
        assert_eq!(
            visible_message(r#"{"message":"say \"hi\" now"#).as_deref(),
            Some("say \"hi\" now")
        );
        assert_eq!(visible_message(r#"{"message":"ша"#).as_deref(), Some("ша"));
    }

    #[test]
    fn a_quote_being_escaped_does_not_end_the_message_early() {
        assert_eq!(visible_message(r#"{"message":"a\"#).as_deref(), Some("a"));
    }

    #[test]
    fn only_what_is_new_is_handed_over_each_time() {
        let mut a = Assembler::new(Protocol::OpenAi);

        let first = a
            .feed("data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"message\\\":\\\"Hel\"}}]}\n");
        assert_eq!(first.as_deref(), Some("Hel"));

        let second = a.feed("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n");
        assert_eq!(
            second.as_deref(),
            Some("lo"),
            "the whole message was re-sent instead of the new part"
        );

        let nothing = a.feed("data: {\"choices\":[{\"delta\":{}}]}\n");
        assert_eq!(nothing, None);
    }

    #[test]
    fn a_turn_that_only_carries_actions_shows_nothing_and_still_parses() {
        let mut a = Assembler::new(Protocol::OpenAi);
        let shown = a.feed(
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"is_done\\\":false,\\\"actions\\\":[]}\"}}]}\n",
        );

        assert_eq!(shown, None);
        let (answer, _) = a.finish();
        assert_eq!(answer, r#"{"is_done":false,"actions":[]}"#);
    }
}
