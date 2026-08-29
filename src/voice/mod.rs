pub mod mic;
pub mod speech;
pub mod state;
pub mod stt;
pub mod tts;
pub mod vad;
pub mod wav;

use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::core::task::Choice;
use crate::error::{JumabekError, JumabekResult};
use crate::interfaces::UserInterface;
use crate::voice::mic::Mic;
use crate::voice::state::VoiceGate;
use crate::voice::stt::Stt;
use crate::voice::tts::Tts;

pub struct Voice {
    gate: VoiceGate,
    tts: Tts,
    stt: Stt,
    #[allow(dead_code)]
    mic: Mic,
    utterances: Mutex<UnboundedReceiver<Vec<i16>>>,
    echo_to_terminal: bool,
}

impl Voice {
    pub fn start(
        groq_api_key: impl Into<String>,
        voice_name: Option<String>,
        language: Option<String>,
        echo_to_terminal: bool,
    ) -> JumabekResult<Self> {
        let gate = VoiceGate::new();
        let (mic, utterances) = Mic::start(gate.clone())?;

        Ok(Voice {
            tts: Tts::new(gate.clone(), voice_name),
            stt: Stt::new(groq_api_key, language)?,
            mic,
            utterances: Mutex::new(utterances),
            gate,
            echo_to_terminal,
        })
    }

    async fn say(&self, text: &str) -> JumabekResult<()> {
        let spoken = speech::to_speakable(text);
        if spoken.is_empty() {
            return Ok(());
        }

        if self.echo_to_terminal {
            println!("  {}", spoken);
        }

        self.tts.speak(&spoken).await
    }

    async fn listen(&mut self) -> JumabekResult<Option<String>> {
        self.gate.begin_listening();

        if self.echo_to_terminal {
            println!("  · listening");
        }

        let mut utterances = self.utterances.lock().await;

        loop {
            let Some(samples) = utterances.recv().await else {
                return Err(JumabekError::InternalError(
                    "the microphone capture thread stopped".to_string(),
                ));
            };

            if self.echo_to_terminal {
                println!(
                    "  · heard {:.1}s, transcribing",
                    samples.len() as f64 / vad::SAMPLE_RATE as f64
                );
            }

            let text = self.stt.transcribe(&samples).await?;
            if text.trim().is_empty() {
                if self.echo_to_terminal {
                    println!("  · nothing recognisable in that, still listening");
                }
                continue;
            }
            if self.echo_to_terminal {
                println!("  you  {}", text);
            }
            return Ok(Some(text));
        }
    }

    fn first_word(answer: &str) -> Option<String> {
        let cleaned: String = answer
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect();

        cleaned
            .split_whitespace()
            .next()
            .map(|word| word.to_string())
    }

    const SPOKEN_ACTION_LIMIT: usize = 100;

    fn needs_the_keyboard(risk_level: &str) -> bool {
        !matches!(risk_level.trim().to_lowercase().as_str(), "low")
    }

    fn speakable_action(action: &str, description: &str) -> String {
        let action = action.trim();
        if action.chars().count() >= 12 {
            return action.to_string();
        }

        let first = description.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            return action.to_string();
        }

        let mut cut: String = first.chars().take(Self::SPOKEN_ACTION_LIMIT).collect();
        if first.chars().count() > Self::SPOKEN_ACTION_LIMIT || description.lines().count() > 1 {
            cut.push_str(", остальное на экране");
        }

        if action.is_empty() {
            cut
        } else {
            format!("{}. {}", action, cut)
        }
    }

    async fn confirm_on_screen(&mut self) -> JumabekResult<bool> {
        loop {
            print!("  allow? [y/N] ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            let line = tokio::task::spawn_blocking(|| {
                let mut buffer = String::new();
                match std::io::stdin().read_line(&mut buffer) {
                    Ok(0) => None,
                    Ok(_) => Some(buffer.trim().to_lowercase()),
                    Err(_) => None,
                }
            })
            .await
            .map_err(|e| JumabekError::InternalError(e.to_string()))?;

            let Some(answer) = line else {
                println!("  denied");
                return Ok(false);
            };

            match answer.as_str() {
                "y" | "yes" | "д" | "да" => {
                    println!("  allowed");
                    return Ok(true);
                }
                "" | "n" | "no" | "н" | "нет" => {
                    println!("  denied");
                    return Ok(false);
                }
                _ => println!("  answer y or n"),
            }
        }
    }

    fn is_affirmative(answer: &str) -> bool {
        Self::first_word(answer).is_some_and(|word| {
            matches!(
                word.as_str(),
                "да" | "ага"
                    | "давай"
                    | "разрешаю"
                    | "конечно"
                    | "yes"
                    | "yeah"
                    | "ok"
                    | "okay"
            )
        })
    }

    fn is_negative(answer: &str) -> bool {
        Self::first_word(answer).is_some_and(|word| {
            matches!(
                word.as_str(),
                "нет" | "не" | "отмена" | "отклоняю" | "стоп" | "no" | "nope" | "cancel"
            )
        })
    }

    fn match_choice(answer: &str, options: &[Choice]) -> Option<String> {
        let cleaned = answer.to_lowercase();

        for (index, option) in options.iter().enumerate() {
            let ordinal = index + 1;
            if cleaned.contains(&ordinal.to_string()) || cleaned.contains(spoken_ordinal(ordinal)) {
                return Some(option.value.clone());
            }
        }

        options
            .iter()
            .find(|option| cleaned.contains(&option.label.to_lowercase()))
            .map(|option| option.value.clone())
    }
}

fn spoken_ordinal(index: usize) -> &'static str {
    match index {
        1 => "перв",
        2 => "втор",
        3 => "трет",
        4 => "четверт",
        5 => "пят",
        _ => "\u{0}",
    }
}

#[async_trait::async_trait]
impl UserInterface for Voice {
    async fn banner(&mut self) -> JumabekResult<()> {
        if self.echo_to_terminal {
            println!();
            println!("  voice mode — speak, or say выход to leave");
            println!("  if it never hears you, run: jumabek mic");
            println!();
        }
        Ok(())
    }

    async fn read_request(&mut self) -> JumabekResult<Option<String>> {
        let text = self.listen().await?;

        if let Some(text) = &text {
            let lowered = text.to_lowercase();
            let trimmed = lowered.trim_end_matches(['.', '!', '?']).trim();
            if ["выход", "выйди", "стоп", "закройся", "exit", "quit"].contains(&trimmed)
            {
                self.gate.idle();
                return Ok(None);
            }
        }

        Ok(text)
    }

    async fn show_response(&mut self, text: &str) -> JumabekResult<()> {
        self.say(&crate::interfaces::markdown::to_speech(text))
            .await
    }

    async fn show_status(&mut self, text: &str) -> JumabekResult<()> {
        if self.echo_to_terminal {
            println!("  · {}", text);
        }
        Ok(())
    }

    async fn show_error(&mut self, text: &str) -> JumabekResult<()> {
        self.say(text).await
    }

    async fn ask_permission(
        &mut self,
        action: &str,
        description: &str,
        risk_level: &str,
    ) -> JumabekResult<bool> {
        println!();
        println!("  permission  {}  {}", risk_level.to_uppercase(), action);
        for line in description.lines() {
            println!("  {}", line);
        }
        println!();

        let spoken_action = Self::speakable_action(action, description);

        if Self::needs_the_keyboard(risk_level) {
            self.say(&format!(
                "Нужно разрешение, уровень риска {}. {}. Подтверди на экране.",
                risk_level, spoken_action
            ))
            .await?;

            return self.confirm_on_screen().await;
        }

        self.say(&format!(
            "Нужно разрешение, уровень риска {}. {}. Разрешить?",
            risk_level, spoken_action
        ))
        .await?;

        for _ in 0..3 {
            let Some(answer) = self.listen().await? else {
                return Ok(false);
            };

            if Self::is_affirmative(&answer) {
                return Ok(true);
            }
            if Self::is_negative(&answer) {
                return Ok(false);
            }

            self.say("Не понял. Скажи да или нет.").await?;
        }

        self.say("Не расслышал ответа, считаю это отказом.").await?;
        Ok(false)
    }

    async fn prompt_choice(&mut self, message: &str, options: &[Choice]) -> JumabekResult<String> {
        if options.is_empty() {
            return Err(JumabekError::InternalError(
                "prompt_choice called with no options".to_string(),
            ));
        }

        let listed = options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {}", i + 1, o.label))
            .collect::<Vec<_>>()
            .join(". ");

        self.say(&format!("{}. Варианты: {}", message, listed))
            .await?;

        for _ in 0..3 {
            let Some(answer) = self.listen().await? else {
                return Ok(options[0].value.clone());
            };

            if let Some(value) = Self::match_choice(&answer, options) {
                return Ok(value);
            }

            self.say("Не понял. Назови номер варианта.").await?;
        }

        Ok(options[0].value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<Choice> {
        vec![
            Choice::new("в Документах", "C:/Users/sosa/Documents/doc.txt"),
            Choice::new("в Загрузках", "C:/Users/sosa/Downloads/doc.txt"),
        ]
    }

    #[test]
    fn hears_yes_and_no() {
        for yes in ["да", "Да.", "ага", "давай", "разрешаю", "yes", "ok"] {
            assert!(Voice::is_affirmative(yes), "missed yes: {yes}");
            assert!(!Voice::is_negative(yes), "yes read as no: {yes}");
        }

        for no in ["нет", "Нет!", "отмена", "стоп", "no", "cancel"] {
            assert!(Voice::is_negative(no), "missed no: {no}");
            assert!(!Voice::is_affirmative(no), "no read as yes: {no}");
        }
    }

    #[test]
    fn unclear_answers_are_neither() {
        for unclear in ["может быть", "погоди", "что", ""] {
            assert!(!Voice::is_affirmative(unclear), "{unclear}");
            assert!(!Voice::is_negative(unclear), "{unclear}");
        }
    }

    #[test]
    fn only_low_risk_may_be_approved_by_voice() {
        assert!(!Voice::needs_the_keyboard("low"));
        for risky in ["medium", "high", "MEDIUM", " High ", "unknown"] {
            assert!(
                Voice::needs_the_keyboard(risky),
                "{risky} could be approved by a stray sentence the mic picked up"
            );
        }
    }

    #[test]
    fn the_command_itself_is_never_spoken() {
        let command =
            "cat > /tmp/x.cs <<'EOF'\n".to_string() + &"public class A { }\n".repeat(200) + "EOF";
        let spoken = Voice::speakable_action("run a shell command", &command);

        assert!(
            spoken.chars().count() < 200,
            "the whole heredoc was queued for the speaker: {} chars",
            spoken.chars().count()
        );
        assert!(!spoken.contains("public class"), "{spoken}");
    }

    #[test]
    fn a_terse_action_borrows_the_first_line_and_says_where_the_rest_is() {
        let spoken = Voice::speakable_action("run", "npm run build\nnpm test");

        assert!(spoken.contains("npm run build"), "{spoken}");
        assert!(
            spoken.contains("на экране"),
            "no pointer to the screen: {spoken}"
        );
        assert!(
            !spoken.contains("npm test"),
            "spoke past the first line: {spoken}"
        );
    }

    #[test]
    fn a_self_explaining_action_is_spoken_alone() {
        let spoken = Voice::speakable_action("delete 340 old log files", "rm -rf /var/log/old/*");
        assert_eq!(spoken, "delete 340 old log files");
    }

    #[test]
    fn a_yes_later_in_the_sentence_is_not_consent() {
        assert!(!Voice::is_affirmative("нет, не надо, да ну его"));
        assert!(Voice::is_negative("нет, не надо, да ну его"));
    }

    #[test]
    fn picks_a_choice_by_ordinal() {
        assert_eq!(
            Voice::match_choice("второй", &options()).unwrap(),
            "C:/Users/sosa/Downloads/doc.txt"
        );
        assert_eq!(
            Voice::match_choice("давай 1", &options()).unwrap(),
            "C:/Users/sosa/Documents/doc.txt"
        );
    }

    #[test]
    fn picks_a_choice_by_label() {
        assert_eq!(
            Voice::match_choice("тот что в загрузках", &options()).unwrap(),
            "C:/Users/sosa/Downloads/doc.txt"
        );
    }

    #[test]
    fn returns_nothing_when_the_answer_matches_no_option() {
        assert!(Voice::match_choice("не знаю", &options()).is_none());
    }
}
