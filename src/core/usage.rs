use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: Option<u32>,
    pub cache_write: Option<u32>,
}

impl Usage {
    pub fn billed_input(&self) -> u32 {
        self.input + self.cache_read.unwrap_or(0) + self.cache_write.unwrap_or(0)
    }

    pub fn says_anything_about_caching(&self) -> bool {
        self.cache_read.is_some() || self.cache_write.is_some()
    }

    pub fn describe(&self) -> String {
        let mut parts = vec![
            format!("{} in", self.billed_input()),
            format!("{} out", self.output),
        ];

        match (self.cache_read, self.cache_write) {
            (None, None) => parts.push("the provider says nothing about caching".to_string()),
            (read, write) if read.unwrap_or(0) == 0 && write.unwrap_or(0) == 0 => {
                parts.push("nothing cached".to_string())
            }
            (Some(read), Some(write)) => {
                parts.push(format!("{} from cache, {} written", read, write))
            }
            (Some(read), None) => parts.push(format!("{} from cache", read)),
            (None, Some(write)) => parts.push(format!("{} written to cache", write)),
        }

        parts.join(" · ")
    }
}

pub fn parse(body: &str) -> Option<Usage> {
    let raw: serde_json::Value = serde_json::from_str(body).ok()?;
    from_value(raw.get("usage")?)
}

pub fn from_json(usage: Option<&serde_json::Value>) -> Option<Usage> {
    from_value(usage?)
}

fn from_value(usage: &serde_json::Value) -> Option<Usage> {
    let number = |name: &str| usage.get(name).and_then(|n| n.as_u64()).map(|n| n as u32);

    let anthropic_input = number("input_tokens");
    let openai_input = number("prompt_tokens");

    let cache_read = number("cache_read_input_tokens").or_else(|| {
        usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32)
    });

    let cache_write = number("cache_creation_input_tokens");

    let (input, output) = match (anthropic_input, openai_input) {
        (Some(input), _) => (input, number("output_tokens").unwrap_or(0)),
        (None, Some(prompt)) => {
            let cached = cache_read.unwrap_or(0);
            (
                prompt.saturating_sub(cached),
                number("completion_tokens").unwrap_or(0),
            )
        }
        (None, None) => return None,
    };

    Some(Usage {
        input,
        output,
        cache_read,
        cache_write,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openai_answer_is_read() {
        let usage = parse(
            r#"{"choices":[],"usage":{"prompt_tokens":24,"completion_tokens":4,"total_tokens":28}}"#,
        )
        .expect("usage was not found");

        assert_eq!(usage.input, 24);
        assert_eq!(usage.output, 4);
        assert_eq!(usage.billed_input(), 24);
    }

    #[test]
    fn an_openai_answer_that_says_nothing_about_caching_is_not_read_as_zero() {
        let usage = parse(r#"{"usage":{"prompt_tokens":24,"completion_tokens":4}}"#).unwrap();

        assert!(
            !usage.says_anything_about_caching(),
            "silence was read as a cache miss"
        );
        assert!(
            usage.describe().contains("says nothing"),
            "{}",
            usage.describe()
        );
    }

    #[test]
    fn an_openai_provider_that_does_report_cached_tokens_is_read() {
        let usage = parse(
            r#"{"usage":{"prompt_tokens":1024,"completion_tokens":8,
                        "prompt_tokens_details":{"cached_tokens":900}}}"#,
        )
        .unwrap();

        assert_eq!(usage.cache_read, Some(900));
        assert_eq!(
            usage.input, 124,
            "cached tokens were counted twice: prompt_tokens already includes them"
        );
        assert_eq!(usage.billed_input(), 1024);
    }

    #[test]
    fn an_anthropic_answer_writing_the_cache_is_read() {
        let usage = parse(
            r#"{"usage":{"input_tokens":8,"output_tokens":4,
                        "cache_read_input_tokens":0,"cache_creation_input_tokens":3658}}"#,
        )
        .unwrap();

        assert_eq!(usage.input, 8);
        assert_eq!(usage.cache_write, Some(3658));
        assert_eq!(usage.cache_read, Some(0));
        assert_eq!(usage.billed_input(), 3666);
    }

    #[test]
    fn an_anthropic_answer_reading_the_cache_is_read() {
        let usage = parse(
            r#"{"usage":{"input_tokens":8,"output_tokens":4,
                        "cache_read_input_tokens":3658,"cache_creation_input_tokens":0}}"#,
        )
        .unwrap();

        assert_eq!(usage.cache_read, Some(3658));
        assert_eq!(usage.billed_input(), 3666);
        assert!(
            usage.describe().contains("3658 from cache"),
            "{}",
            usage.describe()
        );
    }

    #[test]
    fn a_provider_that_reports_only_what_it_read_is_not_called_silent() {
        let usage = parse(
            r#"{"usage":{"prompt_tokens":19466,"completion_tokens":21,
                        "prompt_tokens_details":{"cached_tokens":18430}}}"#,
        )
        .unwrap();

        assert!(usage.says_anything_about_caching());
        let said = usage.describe();
        assert!(said.contains("18430 from cache"), "{said}");
        assert!(
            !said.contains("says nothing"),
            "a real cache hit was reported as silence: {said}"
        );
    }

    #[test]
    fn an_answer_with_no_usage_at_all_gives_nothing_rather_than_zeroes() {
        assert!(parse(r#"{"choices":[]}"#).is_none());
        assert!(parse(r#"{"usage":{}}"#).is_none());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn a_provider_reporting_zero_caching_is_told_apart_from_one_saying_nothing() {
        let silent = parse(r#"{"usage":{"prompt_tokens":10,"completion_tokens":1}}"#).unwrap();
        let explicit = parse(
            r#"{"usage":{"input_tokens":10,"output_tokens":1,
                        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
        )
        .unwrap();

        assert!(!silent.says_anything_about_caching());
        assert!(explicit.says_anything_about_caching());
        assert!(
            explicit.describe().contains("nothing cached"),
            "{}",
            explicit.describe()
        );
    }
}
