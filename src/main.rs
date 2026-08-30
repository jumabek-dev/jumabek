mod cli_args;
mod configs;
mod core;
mod doctor;
mod error;
mod interfaces;
mod memory;
mod panel;
mod skill_layer;
mod supervisor;
mod token_counter;
mod voice;

use std::sync::Arc;

use clap::Parser;

use cli_args::{Args, Manage, Mode};
use configs::Config;
use core::agent::Agent;
use core::jobs::JobStore;
use core::scheduler::{Notifier, PlainNotifier, Scheduler, SharedNotifier};
use error::{JumabekError, JumabekResult};
use interfaces::UserInterface;
use interfaces::cli::Cli;
use memory::Memory;
use skill_layer::{SkillRegistry, loader};
use supervisor::Supervisor;
use voice::Voice;

enum Command {
    Task(String),
    Switch(Mode),
    Quit,
    Unknown(String),
}

#[tokio::main]
async fn main() -> JumabekResult<()> {
    let args = Args::parse();

    if let Some(command) = &args.command {
        return manage(command).await;
    }

    if let Some(flag) = args.flag_like_task() {
        return Err(JumabekError::ConfigError(format!(
            "'{}' was read as the text of a task, not as an option. Anything after `--` is \
             treated as a task. For voice mode use: jumabek --voice",
            flag
        )));
    }

    let (mut config, config_path) = Config::load()?;

    let prompt_status = core::prompt_version::reconcile(&config.system_prompt_file);
    if prompt_status == core::prompt_version::Status::Updated {
        config.system_prompt = core::prompt_version::RELEASE.to_string();
    }

    let mut mode = match args.requested_mode() {
        Some(mode) => mode,
        None => match config.interface_mode()? {
            core::task::InterfaceMode::Cli => Mode::Cli,
            core::task::InterfaceMode::Voice => Mode::Voice,
        },
    };

    let one_shot = args.one_shot_task();
    let (mut ui, notifier) = build_ui(mode, &config)?;
    let notifier = Arc::new(SharedNotifier::new(notifier));

    if one_shot.is_none() {
        ui.banner().await?;
        ui.show_status(&format!("config {}", config_path.display()))
            .await?;
        ui.show_status(&format!("mode {}", mode.as_str())).await?;
        ui.show_status(&format!("{} · {}", config.llm.base_uri, config.llm.model))
            .await?;

        if let Some(note) = prompt_status.note() {
            ui.show_status(&note).await?;
        }
    }

    let watchdog = Supervisor::open()?;
    watchdog.log_event(&format!("startup in {} mode", mode.as_str()));

    let mut memory = Memory::open(&config.db_path(), mode.as_interface_mode().as_str()).await?;

    if config.memory.retrieval {
        if one_shot.is_none() {
            ui.show_status("memory · starting the local embedding model")
                .await?;
        }

        match memory.start_retrieval().await {
            Ok(0) => {
                if one_shot.is_none() {
                    ui.show_status("memory · retrieval on, every fact already has a vector")
                        .await?;
                }
            }
            Ok(done) => {
                ui.show_status(&format!("memory · retrieval on, {} fact(s) embedded", done))
                    .await?;
            }
            Err(e) => {
                ui.show_error(&format!("{} Carrying on with every fact loaded.", e))
                    .await?;
            }
        }
    }
    let mut registry = SkillRegistry::new();
    let skill_timeout = std::time::Duration::from_secs(config.agent.skill_timeout_sec);
    let loaded = loader::load_default(&mut registry, skill_timeout, &|name| {
        config.settings_for_skill(name)
    })
    .await?;

    if one_shot.is_none() {
        ui.show_status(&format!(
            "session {} · {} skill(s): {}",
            memory.session_id(),
            loaded,
            registry
                .list()
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .await?;
    }

    let agent = Agent::new(config.clone(), memory, registry, mode.as_interface_mode())?;
    agent.notify_through(Arc::clone(&notifier) as Arc<dyn Notifier>);

    if let Some(task) = one_shot {
        let result = agent.handle(ui.as_mut(), task).await;
        agent.memory().close().await?;
        return result;
    }

    let scheduler = Arc::new(Scheduler::new(
        Arc::clone(&agent),
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    ));
    Arc::clone(&scheduler).spawn();

    let mut listening: Option<Arc<core::inbox::Inbox>> = None;

    if let Some((inbox, problems)) = core::inbox::Inbox::build(
        &config,
        Arc::clone(&agent),
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    ) {
        for problem in &problems {
            ui.show_error(problem).await?;
        }

        let callers = inbox.callers();
        if callers.is_empty() {
            ui.show_error("inbox is not listening — no usable token")
                .await?;
        } else {
            ui.show_status(&format!(
                "inbox on 127.0.0.1:{} for {}",
                inbox.port(),
                callers.join(", ")
            ))
            .await?;

            let serving = Arc::new(inbox);
            listening = Some(Arc::clone(&serving));
            tokio::spawn(async move {
                if let Err(e) = Arc::clone(&serving).serve().await {
                    eprintln!("[inbox] {}", e);
                }
            });
        }
    }

    core::reload::watch(
        Arc::clone(&agent),
        listening,
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    );

    match agent.jobs().all().await {
        Ok(jobs) if !jobs.is_empty() => {
            ui.show_status(&format!("{} background job(s) scheduled", jobs.len()))
                .await?;
        }
        Ok(_) => {}
        Err(e) => {
            ui.show_error(&format!("cannot read background jobs: {}", e))
                .await?
        }
    }

    while let Some(input) = ui.read_request().await? {
        match parse_command(&input) {
            Command::Quit => break,

            Command::Switch(next) if next == mode => {
                ui.show_status(&format!("already in {} mode", next.as_str()))
                    .await?;
            }

            Command::Switch(next) => match build_ui(next, &config) {
                Ok((replacement, printer)) => {
                    ui = replacement;
                    notifier.replace(printer);
                    mode = next;
                    agent.set_mode(mode.as_interface_mode()).await;
                    ui.show_status(&format!("switched to {} mode", mode.as_str()))
                        .await?;
                }
                Err(e) => {
                    ui.show_error(&format!("cannot switch to {}: {}", next.as_str(), e))
                        .await?;
                }
            },

            Command::Unknown(name) => {
                ui.show_error(&format!(
                    "unknown command '{}'. try /mode cli, /mode voice, /quit",
                    name
                ))
                .await?;
            }

            Command::Task(task) => {
                let outcome = tokio::select! {
                    result = agent.handle(ui.as_mut(), task) => Some(result),
                    _ = tokio::signal::ctrl_c() => None,
                };

                match outcome {
                    Some(Ok(())) => {}
                    Some(Err(e)) => ui.show_error(&e.to_string()).await?,
                    None => {
                        ui.show_error(
                            "Interrupted. Whatever was running keeps going in its own process; \
                             its answer will be discarded.",
                        )
                        .await?;
                    }
                }
            }
        }
    }

    let dropped = agent.agents().running().await;
    if !dropped.is_empty() {
        ui.show_error(&format!(
            "{} sub-agent(s) were still working and die with this process: {}",
            dropped.len(),
            dropped
                .iter()
                .map(|entry| entry.short_id())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .await?;
    }

    agent.memory().close().await?;
    watchdog.log_event("shutdown");
    Ok(())
}

async fn manage(command: &Manage) -> JumabekResult<()> {
    let watchdog = Supervisor::open()?;

    match command {
        Manage::Skills => {
            let dir = loader::skills_dir().ok_or_else(|| {
                JumabekError::ConfigError("cannot resolve home directory".to_string())
            })?;
            let found = loader::discover(&dir)?;

            if found.is_empty() {
                println!("  no skills installed in {}", dir.display());
                return Ok(());
            }

            println!("  {} skill(s) in {}", found.len(), dir.display());
            for path in found {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let name = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let previous = path.with_extension("previous");
                let mark = if previous.exists() {
                    "  (has a previous version)"
                } else {
                    ""
                };
                println!("    {:<24} {:>7} KB{}", name, size / 1024, mark);
            }
        }

        Manage::Remove { name } => {
            let dir = loader::skills_dir().ok_or_else(|| {
                JumabekError::ConfigError("cannot resolve home directory".to_string())
            })?;

            let binary = dir.join(if cfg!(windows) {
                format!("{}.exe", name)
            } else {
                name.clone()
            });

            if !binary.is_file() {
                return Err(JumabekError::ConfigError(format!(
                    "no skill called '{}' in {}",
                    name,
                    dir.display()
                )));
            }

            watchdog.snapshot(&format!("before-removing-{}", name))?;
            std::fs::remove_file(&binary)?;
            watchdog.log_event(&format!("removed skill {}", name));
            println!("  removed {} (a snapshot was taken first)", name);
        }

        Manage::Jobs => {
            let (config, _) = Config::load()?;
            let jobs = JobStore::open(&config.db_path())?.all().await?;

            if jobs.is_empty() {
                println!("  no background jobs");
                return Ok(());
            }

            println!();
            for job in jobs {
                println!("  {:<4} {:<8} {}", job.id, job.state.as_str(), job.name);
                println!("       {}", job.schedule.describe());
                println!(
                    "       ran {} time(s){}",
                    job.runs,
                    match &job.last_run {
                        Some(last) => format!(", last at {}", last),
                        None => ", never".to_string(),
                    }
                );
                if let Some(result) = &job.last_result {
                    println!(
                        "       last result: {}",
                        result.lines().next().unwrap_or("")
                    );
                }
                println!("       may use: {}", job.grant.describe());
                println!();
            }
        }

        Manage::JobStop { id } => {
            let (config, _) = Config::load()?;
            if JobStore::open(&config.db_path())?.remove(*id).await? {
                watchdog.log_event(&format!("stopped background job {}", id));
                println!("  stopped job {}", id);
            } else {
                return Err(JumabekError::ConfigError(format!("there is no job {}", id)));
            }
        }

        Manage::Agents { once } => {
            let (config, _) = Config::load()?;

            if !config.inbox.enabled {
                return Err(JumabekError::ConfigError(
                    "watching agents needs the inbox open — set [inbox] enabled = true".to_string(),
                ));
            }

            watch_agents(config.inbox.port, *once).await?;
        }

        Manage::Rights => {
            let (config, _) = Config::load()?;
            let written = core::board::Board::open(&config.db_path())?
                .expansions()
                .await?;

            println!();
            if written.is_empty() {
                println!("  nobody has been granted anything beyond what config.toml says");
            } else {
                for line in written {
                    println!("  {}", line);
                }
            }
            println!();
            println!(
                "  ceiling: {} — nothing at runtime can go past it",
                config.grants.ceiling.describe()
            );
            println!();
        }

        Manage::Inbox => {
            let (config, _) = Config::load()?;
            println!();

            if !config.inbox.enabled {
                println!("  off — set [inbox] enabled = true in config.toml to open it");
                println!();
                println!("  It listens on 127.0.0.1 only. Anything that knocks needs a token");
                println!("  from secrets.toml and a grant from config.toml saying what it may do.");
                return Ok(());
            }

            let keyring = core::inbox::keyring::Keyring::build(
                &configs::secrets::inbox_tokens(),
                &config.inbox.grants,
            );

            println!("  on — 127.0.0.1:{}", config.inbox.port);
            println!("  ask timeout {}s", config.inbox.ask_timeout_sec);
            println!();

            if keyring.is_empty() {
                println!("  nobody can knock: no usable token");
            } else {
                for name in keyring.names() {
                    let grant = config.inbox.grants.get(name);
                    println!(
                        "  {:<16} {}",
                        name,
                        grant.map(|g| g.describe()).unwrap_or_default()
                    );
                }
            }

            for problem in keyring.problems() {
                println!("  ! {}", problem);
            }

            println!();
            println!("  POST /notify  something happened, no answer expected");
            println!("  POST /ask     a request, the answer comes back in the response");
            println!("  GET  /health  is it listening");
        }

        Manage::Profile => {
            let (config, _) = Config::load()?;
            let memory = Memory::open(&config.db_path(), "cli").await?;
            let facts = memory.known_facts().await?;
            let notes = core::profile::read_notes();
            memory.close().await?;

            if facts.is_empty() && notes.is_empty() {
                println!("  nothing remembered yet");
                return Ok(());
            }

            println!();
            let rendered = memory::facts::render(&facts);
            if !rendered.is_empty() {
                for line in rendered.lines() {
                    println!("  {}", line);
                }
            }
            if !notes.is_empty() {
                println!();
                for line in notes.lines() {
                    println!("  {}", line);
                }
            }
            println!();
            if let Some(path) = core::profile::notes_path() {
                println!("  notes are yours to edit: {}", path.display());
            }
        }

        Manage::ForgetSubject { subject } => {
            let (config, _) = Config::load()?;
            let memory = Memory::open(&config.db_path(), "cli").await?;
            let removed = memory.forget(subject, None).await?;
            memory.close().await?;

            watchdog.log_event(&format!("forgot {} fact(s) about {}", removed, subject));
            println!("  forgot {} fact(s) about '{}'", removed, subject);
        }

        Manage::Mic { seconds } => {
            voice::mic::level_check(*seconds)?;
        }

        Manage::Doctor => {
            let checks = doctor::run().await?;
            doctor::print(&checks);
        }

        Manage::Where => {
            let Some(home) = configs::jumabek_dir() else {
                return Err(JumabekError::ConfigError(
                    "cannot resolve the home directory".to_string(),
                ));
            };
            println!();
            println!("  home       {}", home.display());
            println!("  config     {}", home.join("config.toml").display());
            println!("  prompt     {}", home.join("prompt.md").display());
            println!("  secrets    {}", home.join("secrets.toml").display());
            println!("  database   {}", home.join("jumabek.db").display());
            println!("  skills     {}", home.join("skills").display());
            println!("  backups    {}", home.join("backups").display());
            println!("  workshop   {}", home.join("workshop").display());
            println!("  log        {}", home.join("supervisor.log").display());
            println!();
        }

        Manage::Backups => {
            let snapshots = watchdog.list()?;
            if snapshots.is_empty() {
                println!("  no snapshots yet");
                return Ok(());
            }
            println!("  {} snapshot(s), newest first", snapshots.len());
            for snapshot in snapshots {
                println!(
                    "    {:<40} {:>2} file(s)  {}",
                    snapshot.id,
                    snapshot.files.len(),
                    snapshot.reason
                );
            }
        }

        Manage::Restore { id } => {
            let snapshot = watchdog.restore(id)?;
            println!(
                "  restored {} ({} file(s)) — the previous state was saved first",
                snapshot.id,
                snapshot.files.len()
            );
        }
    }

    Ok(())
}

struct Endpoint {
    client: reqwest::Client,
    url: String,
}

impl panel::AsyncPoll for Endpoint {
    async fn once(&mut self) -> Result<panel::Running, String> {
        ask_for_agents(&self.client, &self.url).await
    }
}

async fn ask_for_agents(client: &reqwest::Client, url: &str) -> Result<panel::Running, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach {}: {}", url, e))?;

    response
        .json::<panel::Running>()
        .await
        .map_err(|e| format!("{} answered something unreadable: {}", url, e))
}

fn draw_agents(entries: &[core::agents::AgentEntry]) {
    println!();

    if entries.is_empty() {
        println!("  nothing running");
        println!();
        return;
    }

    println!(
        "  {:<10} {:<19} {:>7} {:>6}  DOING",
        "AGENT", "STATE", "ITER", "TIME"
    );

    for entry in entries {
        let indent = "  ".repeat(entry.depth as usize);
        println!(
            "  {:<10} {:<19} {:>7} {:>5}s  {}{}",
            entry.short_id(),
            entry.state.id(),
            format!("{}/{}", entry.iteration, entry.max_iterations),
            entry.seconds,
            indent,
            entry.doing
        );
        println!("  {:<10} {}{}", "", indent, cut(&entry.task, 70));
    }

    println!();
}

fn cut(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &flat[..at]),
        None => flat,
    }
}

async fn watch_agents(port: u16, once: bool) -> JumabekResult<()> {
    let url = format!("http://127.0.0.1:{}/agents", port);
    let client = reqwest::Client::new();

    if once {
        let first = ask_for_agents(&client, &url)
            .await
            .map_err(|why| JumabekError::ConfigError(format!("{} — is jumabek running?", why)))?;
        draw_agents(&first.agents);
        return Ok(());
    }

    panel::watch(url.clone(), Endpoint { client, url }).await
}

fn parse_command(input: &str) -> Command {
    let trimmed = input.trim();

    let Some(rest) = trimmed.strip_prefix('/') else {
        return Command::Task(trimmed.to_string());
    };

    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or("").to_lowercase();
    let argument = parts.next().unwrap_or("");

    match name.as_str() {
        "quit" | "exit" | "q" => Command::Quit,
        "mode" => match Mode::parse(argument) {
            Some(mode) => Command::Switch(mode),
            None => Command::Unknown(format!("mode {}", argument)),
        },
        "cli" | "voice" => match Mode::parse(&name) {
            Some(mode) => Command::Switch(mode),
            None => Command::Unknown(name),
        },
        other => Command::Unknown(other.to_string()),
    }
}

fn build_ui(
    mode: Mode,
    config: &Config,
) -> JumabekResult<(Box<dyn UserInterface>, Arc<dyn Notifier>)> {
    match mode {
        Mode::Cli => {
            let mut cli = Cli::new()?;
            let notifier = cli
                .notifier()
                .unwrap_or_else(|| Arc::new(PlainNotifier) as Arc<dyn Notifier>);
            Ok((Box::new(cli), notifier))
        }
        Mode::Voice => Ok((
            Box::new(build_voice(config)?),
            Arc::new(PlainNotifier) as Arc<dyn Notifier>,
        )),
    }
}

fn build_voice(config: &Config) -> JumabekResult<Voice> {
    let key = configs::secrets::groq_api_key()?.ok_or_else(|| {
        JumabekError::ConfigError(
            "voice mode needs a Groq key: set JUMABEK_GROQ_API_KEY, \
             or add [voice] groq_api_key = \"...\" to secrets.toml"
                .to_string(),
        )
    })?;

    Voice::start(
        key,
        config.agent.voice_name.clone(),
        Some(config.agent.language.clone()),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_a_task() {
        assert!(matches!(
            parse_command("сколько файлов"),
            Command::Task(t) if t == "сколько файлов"
        ));
    }

    #[test]
    fn slash_commands_switch_modes() {
        assert!(matches!(
            parse_command("/mode voice"),
            Command::Switch(Mode::Voice)
        ));
        assert!(matches!(
            parse_command("/voice"),
            Command::Switch(Mode::Voice)
        ));
        assert!(matches!(parse_command("/cli"), Command::Switch(Mode::Cli)));
    }

    #[test]
    fn slash_quit_exits() {
        assert!(matches!(parse_command("/quit"), Command::Quit));
        assert!(matches!(parse_command("/q"), Command::Quit));
    }

    #[test]
    fn unknown_slash_commands_are_reported() {
        assert!(matches!(parse_command("/telepathy"), Command::Unknown(_)));
        assert!(matches!(
            parse_command("/mode telepathy"),
            Command::Unknown(_)
        ));
    }

    #[test]
    fn a_path_is_not_mistaken_for_a_command() {
        assert!(matches!(
            parse_command("покажи /etc/hosts"),
            Command::Task(_)
        ));
    }
}
