pub mod cli;
pub mod markdown;

use crate::core::task::Choice;
use crate::error::JumabekResult;

#[async_trait::async_trait]
pub trait UserInterface: Send + Sync {
    async fn banner(&mut self) -> JumabekResult<()> {
        Ok(())
    }

    async fn read_request(&mut self) -> JumabekResult<Option<String>>;

    async fn show_response(&mut self, text: &str) -> JumabekResult<()>;

    /// An answer has started arriving. Everything between this and `stream_end`
    /// is a partial message that will be repainted as more of it lands.
    async fn stream_begin(&mut self) -> JumabekResult<()> {
        Ok(())
    }

    /// The whole message as it stands, not just what is new.
    async fn stream_update(&mut self, _whole: &str) -> JumabekResult<()> {
        Ok(())
    }

    /// `Some(text)` repaints one last time with the final wording and leaves it on
    /// screen; `None` wipes the block, for an answer that turned out not to be one.
    async fn stream_end(&mut self, _keep: Option<&str>) -> JumabekResult<()> {
        Ok(())
    }

    /// Whether a streamed answer is drawn as it arrives. When false the caller
    /// prints the finished message the usual way instead.
    fn streams(&self) -> bool {
        false
    }

    async fn show_status(&mut self, text: &str) -> JumabekResult<()>;

    async fn show_error(&mut self, text: &str) -> JumabekResult<()>;

    async fn ask_permission(
        &mut self,
        action: &str,
        description: &str,
        risk_level: &str,
    ) -> JumabekResult<bool>;

    async fn prompt_choice(&mut self, message: &str, options: &[Choice]) -> JumabekResult<String>;
}
