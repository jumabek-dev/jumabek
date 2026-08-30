use chrono::Local;
use jumabek_sdk::{SkillError, SkillOutput};

use crate::configs::Config;
use crate::core::agents::{AgentEntry, AgentRegistry, State as AgentState};
use crate::core::context::ContextBuilder;
use crate::core::intelligence::{Level, Reason, Standing};
use crate::core::jobs::{JobStore, NewJob, Schedule, State};
use crate::core::languages::Language;
use crate::core::llm::{LlmClient, RequestTarget};
use crate::core::planner;
use crate::core::profile;
use crate::core::safety;
use crate::core::scheduler::Notifier;
use crate::core::self_improvement::{Chunk, Outcome, Progress, SelfImprovement};
use crate::core::task::{
    ActionType, AgentResponse, Constraints, Grant, InterfaceMode, Origin, SystemInfo, TaskObject,
    TaskObjectSkill, TaskObjectSkillMethod,
};
use crate::error::{JumabekError, JumabekResult};
use crate::interfaces::UserInterface;
use crate::memory::{Memory, NewMessage, Role};
use crate::skill_layer::SkillRegistry;
use crate::skill_layer::rpc_client::SkillRpcClient;
use jumabek_sdk::SkillModule;
use std::sync::Arc;
use tokio::sync::RwLock;

const INDEXED_CONTENT_LIMIT: usize = 2_000;

const SKILL_METHOD_BUDGET: usize = 2_000;

const REPORTS_KEPT: usize = 20;

const PARSE_RETRIES: u32 = 2;

const PARSE_CORRECTION: &str = "Your previous answer could not be read as an agent response and \
     was discarded. Answer the same request again. Reply with one JSON object and nothing else: no \
     prose before or after it, no markdown fence. If the last answer was cut off, make this one \
     shorter.";

const STALL_CORRECTION: &str = "Your last answer said is_done: false but sent no actions, so \
     nothing actually ran — a message like \"one moment\" or \"checking now\" is not itself an \
     action. Either send a real action (ExecuteModule, PromptToUser, SpawnAgent, ...) this turn, \
     or set is_done: true if you are actually finished.";

const CAPABILITIES: [&str; 16] = [
    "ExecuteModule",
    "PermissionRequest",
    "PromptToUser",
    "RequestData",
    "Remember",
    "Forget",
    "RequestInboxKey",
    "SpawnAgent",
    "PostToBoard",
    "AskAgent",
    "RequestGrant",
    "ScheduleJob",
    "ManageJobs",
    "GenerateChunk",
    "Switch",
    "RespondToUser",
];

const MAX_DEPTH: u32 = 2;

const CIRCLING_REPEATS: usize = 3;

const CIRCLING_FALLBACK_PERCENT: u32 = 85;

const STALL_FINGERPRINT: &str = "__stall__";

fn is_stall(response: &AgentResponse) -> bool {
    response.actions.is_empty() && !response.is_done
}

fn execute_fingerprint(response: &AgentResponse) -> Option<String> {
    if is_stall(response) {
        return Some(STALL_FINGERPRINT.to_string());
    }

    let mut calls: Vec<String> = response
        .actions
        .iter()
        .filter_map(|action| match action {
            ActionType::ExecuteModule {
                module,
                method,
                args,
                ..
            } => Some(format!("{module}::{method}::{args}")),
            _ => None,
        })
        .collect();

    if calls.is_empty() {
        return None;
    }

    calls.sort();
    Some(calls.join("\n"))
}

fn is_circling(recent: &std::collections::VecDeque<String>) -> bool {
    recent.len() >= CIRCLING_REPEATS && recent.iter().all(|call| call == &recent[0])
}

fn methods_size(skill: &dyn SkillModule) -> usize {
    skill
        .available_methods()
        .iter()
        .map(|m| m.method.len() + m.description.len() + m.args_description.len())
        .sum()
}

fn within_budget(registry: &SkillRegistry) -> std::collections::HashSet<String> {
    let mut sized: Vec<(String, usize)> = registry
        .all()
        .map(|skill| (skill.get_metadata().name.clone(), methods_size(skill)))
        .collect();

    sized.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut spent = 0;
    let mut chosen = std::collections::HashSet::new();

    for (name, size) in sized {
        if spent + size > SKILL_METHOD_BUDGET {
            continue;
        }
        spent += size;
        chosen.insert(name);
    }

    chosen
}

fn model_for(config: &Config, level: Level) -> String {
    let named = config.llm.intelligence.model(level);
    if named.is_empty() {
        config.llm.model.clone()
    } else {
        named.to_string()
    }
}

fn context_limit_for(config: &Config, level: Level) -> u32 {
    config
        .llm
        .intelligence
        .endpoint(level)
        .and_then(|e| e.context_token_limit)
        .unwrap_or(config.llm.context_token_limit)
}

fn target_for(config: &Config, level: Level) -> RequestTarget {
    let overrides = config.llm.intelligence.endpoint(level);

    let base_uri = overrides
        .and_then(|e| e.base_uri.as_deref())
        .unwrap_or(&config.llm.base_uri);
    let reasoning_effort = overrides
        .and_then(|e| e.reasoning_effort.as_deref())
        .unwrap_or(&config.llm.reasoning_effort);
    let structured_output = overrides.and_then(|e| e.structured_output).unwrap_or(true);
    let protocol_raw = overrides
        .and_then(|e| e.protocol.as_deref())
        .unwrap_or(&config.llm.protocol);
    let protocol = crate::core::llm::Protocol::parse(protocol_raw).unwrap_or_default();
    let max_tokens = overrides
        .and_then(|e| e.max_tokens)
        .unwrap_or(config.llm.max_tokens);

    RequestTarget {
        protocol,
        model: model_for(config, level),
        endpoint: crate::core::llm::chat_endpoint(base_uri, protocol),
        api_key: config.api_key_for(level).to_string(),
        reasoning_effort: reasoning_effort.trim().to_string(),
        structured_output,
        max_tokens,
    }
}

pub struct Agent {
    config: RwLock<Config>,
    memory: Memory,
    jobs: JobStore,
    registry: RwLock<SkillRegistry>,
    engine: SelfImprovement,
    expanded: RwLock<std::collections::HashSet<String>>,
    llm: RwLock<LlmClient>,
    context: RwLock<ContextBuilder>,
    mode: RwLock<InterfaceMode>,
    intelligence: RwLock<Standing>,
    refund_iteration: RwLock<bool>,
    session_id: String,
    agents: Arc<AgentRegistry>,
    me: std::sync::OnceLock<std::sync::Weak<Agent>>,
    notifier: std::sync::RwLock<Arc<dyn Notifier>>,
    reports: RwLock<Vec<Letter>>,
    board: crate::core::board::Board,
    discussions: RwLock<std::collections::HashMap<String, u32>>,
    memberships: RwLock<std::collections::HashMap<String, String>>,
    project: RwLock<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct Letter {
    pub to: String,
    pub text: String,
    pub narrow: Option<Grant>,
    pub widen: Option<Grant>,
}

impl Letter {
    fn plain(to: &str, text: String) -> Letter {
        Letter {
            to: to.to_string(),
            text,
            narrow: None,
            widen: None,
        }
    }
}

enum StepOutcome {
    Continue(String),
    Finished,
    Aborted(String),
}

impl Agent {
    pub fn new(
        config: Config,
        memory: Memory,
        registry: SkillRegistry,
        mode: InterfaceMode,
    ) -> JumabekResult<Arc<Self>> {
        let llm = LlmClient::new(&config)?;
        let context =
            ContextBuilder::new(config.system_prompt.clone(), config.llm.context_token_limit);
        let jobs = JobStore::open(&config.db_path())?;
        let board = crate::core::board::Board::open(&config.db_path())?;
        let starting = config.llm.intelligence.starting_level();
        let starting_model = model_for(&config, starting);

        let agent = Arc::new(Agent {
            config: RwLock::new(config),
            memory,
            jobs,
            registry: RwLock::new(registry),
            engine: SelfImprovement::new(),
            expanded: RwLock::new(std::collections::HashSet::new()),
            llm: RwLock::new(llm),
            context: RwLock::new(context),
            mode: RwLock::new(mode),
            intelligence: RwLock::new(Standing {
                level: starting,
                model: starting_model,
                changed_from: None,
                why: None,
                reason: None,
            }),
            refund_iteration: RwLock::new(false),
            session_id: uuid::Uuid::new_v4().to_string(),
            agents: Arc::new(AgentRegistry::new()),
            me: std::sync::OnceLock::new(),
            notifier: std::sync::RwLock::new(Arc::new(crate::core::scheduler::PlainNotifier)),
            reports: RwLock::new(Vec::new()),
            board,
            discussions: RwLock::new(std::collections::HashMap::new()),
            memberships: RwLock::new(std::collections::HashMap::new()),
            project: RwLock::new(None),
        });

        let _ = agent.me.set(Arc::downgrade(&agent));
        Ok(agent)
    }

    pub fn notify_through(&self, notifier: Arc<dyn Notifier>) {
        if let Ok(mut current) = self.notifier.write() {
            *current = notifier;
        }
    }

    fn tell(&self, text: String) {
        let notifier = match self.notifier.read() {
            Ok(current) => Arc::clone(&current),
            Err(_) => return,
        };
        notifier.notify(text);
    }

    fn shared(&self) -> Option<Arc<Agent>> {
        self.me.get().and_then(std::sync::Weak::upgrade)
    }

    async fn leave_report(&self, for_agent: &str, text: String) {
        self.post_letter(Letter::plain(for_agent, text)).await;
    }

    async fn post_letter(&self, letter: Letter) {
        let mut waiting = self.reports.write().await;
        queue_report(&mut waiting, letter);
    }

    async fn take_reports(&self, for_agent: &str) -> Vec<Letter> {
        let mut waiting = self.reports.write().await;
        drain_reports(&mut waiting, for_agent)
    }

    fn notifier_handle(&self) -> Arc<dyn Notifier> {
        match self.notifier.read() {
            Ok(current) => Arc::clone(&current),
            Err(_) => Arc::new(crate::core::scheduler::PlainNotifier),
        }
    }

    pub async fn reload(&self) -> JumabekResult<Vec<String>> {
        let (fresh, _) = Config::load()?;
        let mut changed: Vec<String> = Vec::new();

        {
            let current = self.config.read().await;

            if current.agent.max_iterations != fresh.agent.max_iterations {
                changed.push(format!(
                    "max_iterations {} -> {}",
                    current.agent.max_iterations, fresh.agent.max_iterations
                ));
            }
            if current.agent.max_fix_iterations != fresh.agent.max_fix_iterations {
                changed.push(format!(
                    "max_fix_iterations {} -> {}",
                    current.agent.max_fix_iterations, fresh.agent.max_fix_iterations
                ));
            }
            if current.agent.carry_over_messages != fresh.agent.carry_over_messages {
                changed.push(format!(
                    "carry_over_messages {} -> {}",
                    current.agent.carry_over_messages, fresh.agent.carry_over_messages
                ));
            }
            if current.llm.model != fresh.llm.model {
                changed.push(format!(
                    "model {} -> {}",
                    current.llm.model, fresh.llm.model
                ));
            }
            if current.llm.base_uri != fresh.llm.base_uri {
                changed.push(format!(
                    "endpoint {} -> {}",
                    current.llm.base_uri, fresh.llm.base_uri
                ));
            }
            if current.api_key != fresh.api_key {
                changed.push("api key".to_string());
            }
            if current.system_prompt != fresh.system_prompt {
                changed.push(format!(
                    "prompt {} -> {} characters",
                    current.system_prompt.chars().count(),
                    fresh.system_prompt.chars().count()
                ));
            }

            if current.memory.db_path != fresh.memory.db_path {
                changed.push("db_path changed — restart to use it".to_string());
            }
            if current.inbox.enabled != fresh.inbox.enabled
                || current.inbox.port != fresh.inbox.port
            {
                changed.push("inbox port or switch changed — restart to rebind".to_string());
            }
        }

        let llm = LlmClient::new(&fresh)?;
        let context =
            ContextBuilder::new(fresh.system_prompt.clone(), fresh.llm.context_token_limit);

        changed.extend(self.respawn_changed_skills(&fresh).await);

        *self.llm.write().await = llm;
        *self.context.write().await = context;
        *self.config.write().await = fresh;

        Ok(changed)
    }

    async fn respawn_changed_skills(&self, fresh: &Config) -> Vec<String> {
        let installed: Vec<(String, std::path::PathBuf)> = {
            let registry = self.registry.read().await;
            registry
                .all()
                .filter_map(|skill| {
                    let name = skill.get_metadata().name.clone();
                    crate::skill_layer::loader::binary_for(&name).map(|path| (name, path))
                })
                .collect()
        };

        let mut restarted = Vec::new();

        for (name, path) in installed {
            let wanted = fresh.settings_for_skill(&name);
            let current = self.config.read().await.settings_for_skill(&name);

            if current == wanted {
                continue;
            }

            match SkillRpcClient::spawn_with_settings(&path, wanted).await {
                Ok(client) => {
                    self.registry
                        .write()
                        .await
                        .register(Box::new(client) as Box<dyn SkillModule>);
                    restarted.push(format!("{} restarted with new settings", name));
                }
                Err(e) => restarted.push(format!("{} could not be restarted: {}", name, e)),
            }
        }

        restarted
    }

    pub async fn levels_enabled(&self) -> bool {
        self.config.read().await.llm.intelligence.enabled()
    }

    pub async fn level(&self) -> Level {
        self.intelligence.read().await.level
    }

    async fn move_to(&self, level: Level, reason: Reason) -> bool {
        if !self.levels_enabled().await {
            return false;
        }

        let mut standing = self.intelligence.write().await;
        if standing.level == level {
            return false;
        }

        if level < standing.level && !reason.may_go_down() {
            return false;
        }

        let from = standing.level;
        standing.changed_from = Some(from);
        standing.why = Some(reason.explain().to_string());
        standing.reason = Some(reason);
        standing.level = level;
        standing.model = model_for(&*self.config.read().await, level);
        drop(standing);

        if reason.refunds_the_iteration() {
            *self.refund_iteration.write().await = true;
        }

        true
    }

    async fn escalate(&self, ui: &mut dyn UserInterface, reason: Reason) -> JumabekResult<()> {
        let target = reason.escalation_from(self.level().await);

        if self.move_to(target, reason).await {
            let standing = self.intelligence.read().await;
            ui.show_status(&format!(
                "intelligence {} · {}",
                standing.model,
                reason.explain()
            ))
            .await?;
        }

        Ok(())
    }

    async fn reset_level(&self, task: &TaskObject) {
        if !self.levels_enabled().await {
            return;
        }

        let unattended = task.grant.is_some() || task.origin.is_some();
        let wanted = if unattended {
            Level::Low
        } else {
            self.config.read().await.llm.intelligence.starting_level()
        };

        let reason = if unattended {
            Reason::NobodyWatching
        } else {
            Reason::TaskFinished
        };

        self.move_to(wanted, reason).await;
        let mut standing = self.intelligence.write().await;
        standing.changed_from = None;
        standing.why = None;
        standing.reason = None;
        drop(standing);
        *self.refund_iteration.write().await = false;
    }

    async fn standing_for_task(&self) -> Option<Standing> {
        if !self.levels_enabled().await {
            return None;
        }

        let mut standing = self.intelligence.write().await;
        let snapshot = standing.clone();
        standing.changed_from = None;
        standing.why = None;
        standing.reason = None;
        Some(snapshot)
    }

    async fn current_model(&self) -> Option<String> {
        if !self.levels_enabled().await {
            return None;
        }
        Some(self.intelligence.read().await.model.clone())
    }

    pub async fn inbox_grants(
        &self,
    ) -> std::collections::BTreeMap<String, crate::core::task::Grant> {
        self.config.read().await.inbox.grants.clone()
    }

    pub async fn set_mode(&self, mode: InterfaceMode) {
        *self.mode.write().await = mode;
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    pub fn board(&self) -> &crate::core::board::Board {
        &self.board
    }

    pub fn jobs(&self) -> &JobStore {
        &self.jobs
    }

    pub async fn run_detached(
        &self,
        task: String,
        grant: Grant,
        origin: Origin,
    ) -> JumabekResult<String> {
        let mut detached = crate::core::scheduler::detached_ui();
        let mut job_task = self.new_task(&uuid::Uuid::new_v4().to_string(), task).await;
        job_task.agent_id = format!("inbox:{}", origin.source);
        job_task.grant = Some(grant);
        job_task.origin = Some(origin);
        self.run(&mut detached, job_task).await
    }

    pub async fn run_job(
        &self,
        ui: &mut dyn UserInterface,
        task: String,
        grant: Grant,
        job_id: i64,
    ) -> JumabekResult<String> {
        let mut job_task = self.new_task(&uuid::Uuid::new_v4().to_string(), task).await;
        job_task.agent_id = format!("job:{}", job_id);
        job_task.grant = Some(grant);
        self.run(ui, job_task).await
    }

    pub async fn handle(&self, ui: &mut dyn UserInterface, request: String) -> JumabekResult<()> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let task = self.new_task(&task_id, request).await;
        self.run(ui, task).await.map(|_| ())
    }

    fn run<'a>(
        &'a self,
        ui: &'a mut dyn UserInterface,
        task: TaskObject,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = JumabekResult<String>> + Send + 'a>>
    {
        let caller = task.agent_id.clone();
        let parent = crate::skill_layer::current_caller();
        Box::pin(crate::skill_layer::CALLER.scope(caller, self.watched(ui, task, parent)))
    }

    async fn watched(
        &self,
        ui: &mut dyn UserInterface,
        task: TaskObject,
        parent: Option<String>,
    ) -> JumabekResult<String> {
        let agent_id = task.agent_id.clone();

        self.agents
            .register(
                AgentEntry::new(&agent_id, &task.message)
                    .under(parent, task.depth)
                    .belonging(
                        task.group.as_ref().map(|group| group.id.clone()),
                        task.role.clone(),
                    )
                    .allowed(task.constraints.max_iterations),
            )
            .await;

        let outcome = self.run_loop(ui, task).await;

        self.agents
            .finished(
                &agent_id,
                match outcome {
                    Ok(_) => AgentState::Finished,
                    Err(_) => AgentState::Failed,
                },
            )
            .await;

        outcome
    }

    pub fn agents(&self) -> Arc<AgentRegistry> {
        Arc::clone(&self.agents)
    }

    async fn run_loop(
        &self,
        ui: &mut dyn UserInterface,
        mut task: TaskObject,
    ) -> JumabekResult<String> {
        let step = self.config.read().await.agent.max_iterations;
        let mut budget = step;
        let mut last_message = String::new();
        let mut recent_calls: std::collections::VecDeque<String> =
            std::collections::VecDeque::new();

        if task.depth == 0 {
            self.reset_level(&task).await;
        }

        loop {
            task.intelligence = self.standing_for_task().await;
            self.agents.iteration(&task.agent_id, task.iteration).await;
            self.agents.doing(&task.agent_id, "thinking").await;

            if task.group.is_none()
                && let Some(id) = self.group_of(&task.agent_id).await
            {
                task.group = self.group_view(&id).await;
            }

            let delivered = self.take_reports(&task.agent_id).await;
            if !delivered.is_empty() {
                apply_letters(&mut task, &delivered);
                let text = delivered.iter().map(|l| l.text.clone()).collect();
                task.system_response = Some(join_reports(task.system_response.take(), text));
            }

            if let Some(stop) = self.spend_group_iteration(&mut task).await {
                ui.show_status(&stop).await?;
                return Ok(stop);
            }

            let history = self.history_for(&task).await?;
            let profile = self.profile_block(&task).await;
            let level = self.level().await;
            let (context, token_limit) = {
                let config = self.config.read().await;
                let limit = context_limit_for(&config, level);
                (self.context.read().await.rescaled(limit), limit)
            };
            let built = context.build_with_profile(&history, &task, &profile)?;

            if built.trimmed_messages > 0 {
                ui.show_status(&format!(
                    "context {} tokens, trimmed: {} older messages hidden",
                    built.total_tokens, built.trimmed_messages
                ))
                .await?;
            } else if built.total_tokens * 2 > token_limit as usize {
                ui.show_status(&format!(
                    "context {} of {} tokens",
                    built.total_tokens, token_limit
                ))
                .await?;
            }

            let reply = self.ask_until_readable(ui, &built.messages).await?;
            self.log_turn(&task, &reply.response, &reply.raw_content)
                .await?;

            if let Some(counted) = reply.usage {
                let target = target_for(&*self.config.read().await, level);
                if let Err(e) = self
                    .memory
                    .log_usage(
                        &task.task_id,
                        &target.model,
                        target.protocol.id(),
                        &counted,
                        built.total_tokens as u32,
                    )
                    .await
                {
                    self.tell(format!("  x tokens · not recorded: {}", e));
                }

                let far_off = counted.billed_input().abs_diff(built.total_tokens as u32)
                    > built.total_tokens as u32 / 4;
                let cached = counted.cache_read.unwrap_or(0) > 0;

                if far_off || cached {
                    ui.show_status(&format!(
                        "tokens counted {} · guessed {}{}",
                        counted.describe(),
                        built.total_tokens,
                        if counted.says_anything_about_caching() {
                            ""
                        } else {
                            " (this endpoint reports no cache figures)"
                        }
                    ))
                    .await?;
                }
            }

            if !reply.response.message.trim().is_empty() && !is_stall(&reply.response) {
                last_message = reply.response.message.clone();
                if task.depth == 0 {
                    ui.show_response(&reply.response.message).await?;
                } else {
                    ui.show_status(&format!("subagent · {}", first_line(&last_message)))
                        .await?;
                }
            }

            match self.run_actions(ui, &task, &reply.response).await? {
                StepOutcome::Finished => return Ok(last_message),
                StepOutcome::Aborted(reason) => {
                    if task.depth == 0 {
                        ui.show_error(&reason).await?;
                    }
                    return Ok(reason);
                }
                StepOutcome::Continue(system_response) => {
                    let mut escalated = false;

                    if let Some(fingerprint) = execute_fingerprint(&reply.response) {
                        recent_calls.push_back(fingerprint);
                        if recent_calls.len() > CIRCLING_REPEATS {
                            recent_calls.pop_front();
                        }
                        if is_circling(&recent_calls) {
                            self.escalate(ui, Reason::Circling).await?;
                            recent_calls.clear();
                            escalated = true;
                        }
                    } else {
                        recent_calls.clear();
                    }

                    if !escalated
                        && task.iteration * 100 >= budget * CIRCLING_FALLBACK_PERCENT
                        && task.iteration + 1 < budget
                    {
                        self.escalate(ui, Reason::Circling).await?;
                    }

                    if std::mem::take(&mut *self.refund_iteration.write().await) {
                        task.system_response = Some(system_response);
                        continue;
                    }

                    task.iteration += 1;
                    if task.iteration >= budget {
                        if !self.ask_for_more_iterations(ui, &task, budget).await? {
                            return Ok(format!(
                                "stopped at {} iterations without finishing",
                                budget
                            ));
                        }
                        budget += step;
                        task.constraints.max_iterations = budget;
                    }
                    task.system_response = Some(system_response);
                }
            }
        }
    }

    async fn history_for(
        &self,
        task: &TaskObject,
    ) -> JumabekResult<Vec<crate::memory::StoredMessage>> {
        let history = self.memory.current_session().await?;

        if reads_the_whole_session(task) {
            let carried = self
                .memory
                .previous_session_tail(self.config.read().await.agent.carry_over_messages)
                .await?;

            if carried.is_empty() {
                return Ok(history);
            }

            let mut all = carried;
            all.extend(history);
            return Ok(all);
        }

        Ok(own_messages(history, &task.task_id))
    }

    async fn profile_block(&self, task: &TaskObject) -> crate::core::context::Profile {
        let project = self.project.read().await.clone();
        let facts = self
            .memory
            .facts_for(&task.message, project.as_deref())
            .await
            .unwrap_or_default();

        let (pinned, fetched): (Vec<_>, Vec<_>) = facts.into_iter().partition(|fact| fact.pinned);

        let mut stable = profile::block(&pinned, &profile::read_notes());

        let fragment = self.role_prompt(task.role.as_ref()).await;
        if !fragment.trim().is_empty() {
            let named = format!(
                "You are working as the {}.\n{}",
                task.role.as_deref().unwrap_or("agent"),
                fragment.trim()
            );
            stable = if stable.is_empty() {
                named
            } else {
                format!("{}\n\n{}", stable, named)
            };
        }

        crate::core::context::Profile {
            stable,
            volatile: profile::fetched_block(&fetched),
        }
    }

    async fn ask_for_more_iterations(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        used: u32,
    ) -> JumabekResult<bool> {
        if task.grant.is_some() {
            return Ok(false);
        }

        let carry_on = ui
            .ask_permission(
                "keep going",
                &format!(
                    "{} iterations have been used and the task is not finished. \
                     Allowing this grants {} more.",
                    used,
                    self.config.read().await.agent.max_iterations
                ),
                "low",
            )
            .await?;

        let note = if carry_on {
            format!("user granted more iterations past {}", used)
        } else {
            format!("user stopped the task at {} iterations", used)
        };

        self.memory
            .log(NewMessage::new(Role::System, &note).task(&task.task_id))
            .await?;

        if !carry_on {
            ui.show_status(&format!("stopped at {} iterations", used))
                .await?;
        }

        Ok(carry_on)
    }

    async fn ask_until_readable(
        &self,
        ui: &mut dyn UserInterface,
        messages: &[crate::core::task::LlmMessage],
    ) -> JumabekResult<crate::core::llm::LlmReply> {
        let mut attempt = 0;

        loop {
            let mut sent = messages.to_vec();
            if attempt > 0 {
                sent.push(crate::core::task::LlmMessage::new(
                    "system",
                    PARSE_CORRECTION,
                ));
            }

            let target = target_for(&*self.config.read().await, self.level().await);
            match self.llm.read().await.clone().ask_as(&sent, &target).await {
                Ok(reply) => return Ok(reply),
                Err(JumabekError::ParseError(detail)) if attempt < PARSE_RETRIES => {
                    attempt += 1;
                    if attempt >= PARSE_RETRIES {
                        self.escalate(ui, Reason::UnreadableAnswer).await?;
                    }
                    ui.show_status(&format!(
                        "unreadable answer, asking again ({}/{}): {}",
                        attempt,
                        PARSE_RETRIES,
                        first_line(&detail)
                    ))
                    .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn new_task(&self, task_id: &str, request: String) -> TaskObject {
        let (max_iterations, max_fix_iterations) = {
            let config = self.config.read().await;
            (config.agent.max_iterations, config.agent.max_fix_iterations)
        };

        TaskObject {
            task_id: task_id.to_string(),
            agent_id: self.session_id.clone(),
            parent_task_id: None,
            message: request,
            system_info: system_info(),
            system_response: None,
            skills: self.skill_descriptions().await,
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            constraints: Constraints {
                max_iterations,
                max_fix_iterations,
            },
            iteration: 0,
            fix_iteration: 0,
            depth: 0,
            role: None,
            group: None,
            grant: None,
            origin: None,
            intelligence: None,
            interface_mode: *self.mode.read().await,
        }
    }

    async fn new_child_task(
        &self,
        parent: &TaskObject,
        request: &str,
        role: &str,
        group_id: Option<String>,
    ) -> TaskObject {
        let mut child = self
            .new_task(&uuid::Uuid::new_v4().to_string(), request.to_string())
            .await;
        child.agent_id = uuid::Uuid::new_v4().to_string();
        child.parent_task_id = Some(parent.task_id.clone());
        child.depth = parent.depth + 1;

        child.grant = match self.role_grant(role).await {
            Some(from_role) => Some(match &parent.grant {
                Some(inherited) => inherited.narrow(&from_role),
                None => from_role,
            }),
            None => parent.grant.clone(),
        };

        if !role.trim().is_empty() {
            child.role = Some(role.trim().to_string());
        }

        if let Some(id) = group_id {
            self.join_group(&child.agent_id, &id).await;
            child.group = self.group_view(&id).await;
        }

        child
    }

    async fn spend_group_iteration(&self, task: &mut TaskObject) -> Option<String> {
        let id = task.group.as_ref()?.id.clone();

        let group = match self.board.spend(&id).await {
            Ok(Some(group)) => group,
            Ok(None) => return None,
            Err(e) => {
                self.tell(format!("  x board · {}", e));
                return None;
            }
        };

        if let Some(view) = task.group.as_mut() {
            view.iterations_left = group.left();
        }

        if !group.exhausted() {
            return None;
        }

        let first = self.board.close_group(&id).await.unwrap_or(false);
        self.leave_group(&task.agent_id).await;
        task.group = None;

        if first {
            let _ = self
                .board
                .post(
                    &id,
                    &task.agent_id,
                    crate::core::board::EVERYONE,
                    crate::core::board::Kind::Decision,
                    &format!(
                        "the group ran out of its shared {} iterations and was stopped",
                        group.budget
                    ),
                )
                .await;
        }

        Some(format!(
            "the group working on '{}' used all {} of its shared iterations and was stopped. \
             Whatever is on the board is what it got.",
            first_line(&group.goal),
            group.budget
        ))
    }

    async fn use_board(
        &self,
        task: &TaskObject,
        kind: &str,
        to: &str,
        body: &str,
        entry: i64,
        state: &str,
    ) -> String {
        let Some(group) = &task.group else {
            return "[BOARD ERROR] you are not working in a group, so there is no board. \
                    Spawn an agent to open one."
                .to_string();
        };

        if entry > 0 {
            let Some(wanted) = crate::core::board::EntryState::parse(state) else {
                return format!(
                    "[BOARD ERROR] '{}' is not a state. Use claimed or done.",
                    state
                );
            };

            return match self.board.set_state(&group.id, entry, wanted).await {
                Ok(true) => format!("[BOARD] #{} is now {}", entry, wanted.as_str()),
                Ok(false) => format!(
                    "[BOARD ERROR] there is no entry #{} on your group's board",
                    entry
                ),
                Err(e) => format!("[BOARD ERROR] {}", e),
            };
        }

        if body.trim().is_empty() {
            return "[BOARD ERROR] an entry needs a body".to_string();
        }

        let Some(wanted) = crate::core::board::Kind::parse(kind) else {
            return format!(
                "[BOARD ERROR] '{}' is not a kind. Use task, finding, decision or question.",
                kind
            );
        };

        let addressee = if to.trim().is_empty() {
            crate::core::board::EVERYONE
        } else {
            to.trim()
        };

        match self
            .board
            .post(&group.id, &task.agent_id, addressee, wanted, body)
            .await
        {
            Ok(id) => format!("[BOARD] wrote #{} as a {}", id, wanted.as_str()),
            Err(e) => format!("[BOARD ERROR] {}", e),
        }
    }

    async fn ask_agent(&self, task: &TaskObject, to: &str, message: &str) -> String {
        let Some(group) = &task.group else {
            return "[ASK ERROR] you can only talk to agents in your own group, and you are \
                    not in one."
                .to_string();
        };

        if message.trim().is_empty() {
            return "[ASK ERROR] say something".to_string();
        }

        let members = self.agents.snapshot().await;
        let Some(target) = members.iter().find(|entry| {
            entry.group_id.as_deref() == Some(group.id.as_str())
                && entry.agent_id != task.agent_id
                && (entry.agent_id == to.trim() || entry.role.as_deref() == Some(to.trim()))
        }) else {
            return format!(
                "[ASK ERROR] nobody called '{}' is in your group. In it right now: {}",
                to,
                if group.members.is_empty() {
                    "only you".to_string()
                } else {
                    group.members.join(", ")
                }
            );
        };

        let pair = discussion_key(&group.id, &task.agent_id, &target.agent_id);
        let allowed = self.config.read().await.agent.discussion_turns;
        let spent = {
            let mut talking = self.discussions.write().await;
            let count = talking.entry(pair).or_insert(0);
            *count += 1;
            *count
        };

        if spent > allowed {
            let _ = self
                .board
                .post(
                    &group.id,
                    &task.agent_id,
                    crate::core::board::EVERYONE,
                    crate::core::board::Kind::Decision,
                    &format!(
                        "the exchange with {} was closed after {} turns without agreement;                          it goes to whoever spawned us",
                        short_id(&target.agent_id),
                        allowed
                    ),
                )
                .await;

            if let Some(parent) = &target.parent_id {
                self.leave_report(
                    parent,
                    format!(
                        "[GROUP] {} and {} talked past each other for {} turns and were stopped.                          The disagreement is yours to settle; their board has the record.",
                        short_id(&task.agent_id),
                        short_id(&target.agent_id),
                        allowed
                    ),
                )
                .await;
            }

            return format!(
                "[ASK REFUSED] you and {} have used all {} turns of this exchange. It is closed, \
                 a decision is on the board and it has gone up to whoever spawned you. Work with \
                 what you have.",
                short_id(&target.agent_id),
                allowed
            );
        }

        self.post_letter(Letter {
            to: target.agent_id.clone(),
            text: format!(
                "[FROM {}{}] {}\nAnswer with AskAgent to '{}' if it needs an answer, and put \
                 anything the group should keep on the board.",
                short_id(&task.agent_id),
                match &task.role {
                    Some(role) => format!(" the {}", role),
                    None => String::new(),
                },
                message.trim(),
                task.agent_id
            ),
            narrow: task.grant.clone(),
            widen: None,
        })
        .await;

        format!(
            "[ASK] sent to {}. Its answer reaches you on a later turn; {} of {} turns of this \
             exchange are left. It works under your rights as well as its own while it answers.",
            short_id(&target.agent_id),
            allowed.saturating_sub(spent),
            allowed
        )
    }

    async fn expand_grant(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        wanted: &Grant,
        why: &str,
        critical: bool,
    ) -> JumabekResult<String> {
        let ceiling = self.config.read().await.grants.ceiling.clone();
        let verdict = decide_grant(
            task.grant.as_ref(),
            wanted,
            &ceiling,
            critical,
            task.depth == 0,
        );

        if let Some((outcome, by)) = verdict.recorded() {
            self.audit(&task.agent_id, wanted, why, outcome, by).await;
        }

        match verdict {
            Verdict::NotUnderGrant => {
                return Ok(
                    "[RIGHTS] you are not working under a restricted grant; nothing is \
                     being withheld from you."
                        .to_string(),
                );
            }

            Verdict::NothingAsked => {
                return Ok("[RIGHTS ERROR] name what you need".to_string());
            }

            Verdict::AboveCeiling => {
                return Ok(format!(
                    "[RIGHTS REFUSED] {} is above the ceiling set in config.toml, which nothing \
                     at runtime can raise. The ceiling allows {}. Finish without it or report \
                     that you could not.",
                    wanted.describe(),
                    ceiling.describe()
                ));
            }

            Verdict::SentUpward => {
                if let Some(parent) = self.parent_of(&task.agent_id).await {
                    self.leave_report(
                        &parent,
                        format!(
                            "[RIGHTS] {} needs {} and calls it critical: {}. Nobody was there \
                             to ask. Decide it, or put it to the user.",
                            short_id(&task.agent_id),
                            wanted.describe(),
                            why
                        ),
                    )
                    .await;
                }

                return Ok(format!(
                    "[RIGHTS QUEUED] nobody is at the keyboard, so {} has gone up to whoever \
                     spawned you. Carry on with what you have; do not wait.",
                    wanted.describe()
                ));
            }

            Verdict::GrantedByMainAgent => {
                self.widen_grant(task, wanted, "widened to").await;

                return Ok(format!(
                    "[RIGHTS] granted {} — it was inside the ceiling and you did not call it \
                     critical. It applies from your next turn.",
                    wanted.describe()
                ));
            }

            Verdict::PutToTheUser => {}
        }

        {
            let allowed = ui
                .ask_permission(
                    &format!("widen its own rights to {}", wanted.describe()),
                    why,
                    "high",
                )
                .await?;

            self.audit(
                &task.agent_id,
                wanted,
                why,
                if allowed { "granted" } else { "refused" },
                "user",
            )
            .await;

            if !allowed {
                return Ok(format!(
                    "[RIGHTS REFUSED] the user said no to {}. Finish without it.",
                    wanted.describe()
                ));
            }

            self.widen_grant(task, wanted, "the user granted").await;
            Ok(format!("[RIGHTS] granted {}", wanted.describe()))
        }
    }

    async fn widen_grant(&self, task: &TaskObject, wanted: &Grant, said: &str) {
        self.post_letter(Letter {
            to: task.agent_id.clone(),
            text: format!("[RIGHTS] {} {}", said, wanted.describe()),
            narrow: None,
            widen: Some(wanted.clone()),
        })
        .await;
    }

    async fn audit(&self, who: &str, wanted: &Grant, why: &str, verdict: &str, by: &str) {
        if let Err(e) = self
            .board
            .audit(who, &wanted.describe(), why, verdict, by)
            .await
        {
            self.tell(format!("  x rights · the record was not written: {}", e));
        }
    }

    async fn parent_of(&self, agent_id: &str) -> Option<String> {
        self.agents
            .snapshot()
            .await
            .into_iter()
            .find(|entry| entry.agent_id == agent_id)
            .and_then(|entry| entry.parent_id)
    }

    async fn role_exists(&self, name: &str) -> bool {
        self.config.read().await.roles.contains_key(name)
    }

    async fn role_names(&self) -> String {
        let config = self.config.read().await;
        if config.roles.is_empty() {
            return "none are configured".to_string();
        }
        config.roles.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    async fn role_grant(&self, name: &str) -> Option<Grant> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        self.config.read().await.roles.get(name).map(|r| r.grant())
    }

    async fn role_prompt(&self, name: Option<&String>) -> String {
        let Some(name) = name else {
            return String::new();
        };
        self.config
            .read()
            .await
            .roles
            .get(name)
            .map(|role| role.prompt.clone())
            .unwrap_or_default()
    }

    async fn group_for(&self, task: &TaskObject) -> Option<String> {
        if let Some(group) = &task.group {
            return Some(group.id.clone());
        }

        let budget = self.config.read().await.agent.group_iteration_budget;
        if budget == 0 {
            return None;
        }

        let id = format!("g-{}", &task.task_id[..task.task_id.len().min(8)]);
        if let Err(e) = self.board.open_group(&id, &task.message, budget).await {
            self.tell(format!("  x board · could not open a group: {}", e));
            return None;
        }

        self.join_group(&task.agent_id, &id).await;
        Some(id)
    }

    async fn join_group(&self, agent_id: &str, group_id: &str) {
        self.memberships
            .write()
            .await
            .insert(agent_id.to_string(), group_id.to_string());
    }

    async fn group_of(&self, agent_id: &str) -> Option<String> {
        self.memberships.read().await.get(agent_id).cloned()
    }

    async fn leave_group(&self, agent_id: &str) {
        self.memberships.write().await.remove(agent_id);
    }

    async fn group_view(&self, id: &str) -> Option<crate::core::task::GroupView> {
        let group = self.board.group(id).await.ok().flatten()?;
        let left = group.left();
        Some(crate::core::task::GroupView {
            id: group.id,
            goal: group.goal,
            iterations_left: left,
            members: self
                .agents
                .snapshot()
                .await
                .into_iter()
                .filter(|entry| entry.group_id.as_deref() == Some(id))
                .map(|entry| match entry.role {
                    Some(role) => format!("{} ({})", entry.agent_id, role),
                    None => entry.agent_id,
                })
                .collect(),
        })
    }

    async fn skill_descriptions(&self) -> Vec<TaskObjectSkill> {
        let registry = self.registry.read().await;
        let expanded = self.expanded.read().await;

        let affordable = within_budget(&registry);

        registry
            .all()
            .map(|skill| {
                let metadata = skill.get_metadata();
                let show_methods =
                    affordable.contains(&metadata.name) || expanded.contains(&metadata.name);

                TaskObjectSkill {
                    name: metadata.name.clone(),
                    description: metadata.description.clone(),
                    available_methods: if show_methods {
                        skill
                            .available_methods()
                            .into_iter()
                            .map(|m| TaskObjectSkillMethod {
                                method: m.method,
                                description: m.description,
                                args_description: m.args_description,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect()
    }

    async fn expand_skill(&self, name: &str) -> bool {
        if self.registry.read().await.get(name).is_none() {
            return false;
        }
        self.expanded.write().await.insert(name.to_string());
        true
    }

    async fn log_turn(
        &self,
        task: &TaskObject,
        response: &AgentResponse,
        raw_content: &str,
    ) -> JumabekResult<()> {
        let sent = match &task.system_response {
            Some(response) => truncate_for_index(response),
            None => task.message.clone(),
        };
        let task_json = serde_json::to_string(task)
            .map_err(|e| JumabekError::ParseError(format!("cannot encode task object: {}", e)))?;
        let level = task.intelligence.as_ref().map(|s| s.level.id().to_string());
        let change = task
            .intelligence
            .as_ref()
            .and_then(|s| s.reason)
            .map(|r| r.id().to_string());

        self.memory
            .log(
                NewMessage::new(Role::User, sent)
                    .task(&task.task_id)
                    .parent(task.parent_task_id.clone())
                    .raw(task_json),
            )
            .await?;

        self.memory
            .log(
                NewMessage::new(Role::Assistant, response.message.clone())
                    .task(&task.task_id)
                    .parent(task.parent_task_id.clone())
                    .level(level)
                    .level_change(change)
                    .raw(raw_content.to_string()),
            )
            .await?;

        Ok(())
    }

    async fn run_actions(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        response: &AgentResponse,
    ) -> JumabekResult<StepOutcome> {
        let plan = planner::plan(&response.actions);

        if plan.parallel_groups() > 0 {
            ui.show_status(&format!("plan: {}", plan.describe()))
                .await?;
        }

        let mut results: Vec<String> = Vec::new();

        for stage in &plan.stages {
            for action in stage_actions(stage) {
                if let Some(refusal) = refuse_outside_grant(task, action) {
                    return Ok(StepOutcome::Aborted(refusal));
                }
            }

            for action in stage_actions(stage) {
                if let ActionType::ExecuteModule {
                    module,
                    method,
                    args,
                    ..
                } = action
                    && let Some(outcome) = self.check_safety(ui, task, module, method, args).await?
                {
                    return Ok(outcome);
                }
            }

            if let planner::Stage::Parallel(list) = stage {
                self.agents
                    .doing(
                        &task.agent_id,
                        &format!("{} steps at once", stage_actions(stage).len()),
                    )
                    .await;

                match self.run_parallel(ui, task, list).await? {
                    Ok(mut batch) => results.append(&mut batch),
                    Err(outcome) => return Ok(outcome),
                }
                continue;
            }

            let action = stage_actions(stage)[0];
            self.agents.doing(&task.agent_id, &action.label()).await;

            match action {
                ActionType::RespondToUser => {}

                ActionType::ExecuteModule {
                    module,
                    method,
                    args,
                    ..
                } => {
                    self.expand_skill(module).await;
                    ui.show_status(&format!("{} · {}", module, method)).await?;

                    let registry = self.registry.read().await;
                    let Some(skill) = registry.get(module) else {
                        drop(registry);
                        results.push(format!(
                            "[ERROR] unknown skill '{}'. Available: {}",
                            module,
                            self.available_names().await
                        ));
                        continue;
                    };

                    match skill.execute(method, args).await {
                        Ok(output) => {
                            let text = render_output(output);
                            self.memory
                                .log(NewMessage::new(Role::Skill, &text).task(&task.task_id))
                                .await?;
                            results.push(format!("[{}::{}] {}", module, method, text));
                        }
                        Err(err) => {
                            let wrapped = JumabekError::SkillError(err);
                            self.memory
                                .log(
                                    NewMessage::new(Role::Skill, wrapped.to_string())
                                        .task(&task.task_id),
                                )
                                .await?;

                            if !wrapped.is_recoverable() {
                                return Ok(StepOutcome::Aborted(wrapped.to_string()));
                            }
                            results.push(format!("[{}::{}] {}", module, method, wrapped));
                        }
                    }
                }

                ActionType::PermissionRequest {
                    action,
                    description,
                    risk_level,
                } => {
                    self.agents.waiting(&task.agent_id, true).await;
                    let allowed = ui.ask_permission(action, description, risk_level).await?;
                    self.agents.waiting(&task.agent_id, false).await;
                    let verdict = if allowed { "granted" } else { "denied" };

                    self.memory
                        .log(
                            NewMessage::new(
                                Role::System,
                                format!("permission {} for '{}'", verdict, action),
                            )
                            .task(&task.task_id),
                        )
                        .await?;

                    if !allowed {
                        return Ok(StepOutcome::Aborted(format!(
                            "[PERMISSION ERROR] denied: {}. The task cannot continue without it.",
                            action
                        )));
                    }

                    results.push(format!("[PERMISSION] granted: {}", action));
                }

                ActionType::PromptToUser { message, options } => {
                    self.agents.waiting(&task.agent_id, true).await;
                    let answer = if options.is_empty() {
                        ui.show_response(message).await?;
                        match ui.read_request().await? {
                            Some(text) => text,
                            None => {
                                return Ok(StepOutcome::Aborted(
                                    "No answer given, task stopped.".to_string(),
                                ));
                            }
                        }
                    } else {
                        ui.prompt_choice(message, options).await?
                    };
                    self.agents.waiting(&task.agent_id, false).await;

                    self.memory
                        .log(NewMessage::new(Role::User, &answer).task(&task.task_id))
                        .await?;

                    results.push(format!("[USER] {}", answer));
                }

                ActionType::RequestData {
                    source,
                    query,
                    limit,
                } => {
                    if source == "skill" {
                        let name = query.trim();
                        if self.expand_skill(name).await {
                            ui.show_status(&format!("skill · {}", name)).await?;
                            results.push(format!(
                                "[SKILL] the methods of '{}' are now listed in your skills field",
                                name
                            ));
                        } else {
                            results.push(format!(
                                "[ERROR] no skill called '{}'. Available: {}",
                                name,
                                self.available_names().await
                            ));
                        }
                        continue;
                    }

                    if source == "facts" {
                        let named = query.trim().to_lowercase();
                        *self.project.write().await = if named.is_empty() {
                            None
                        } else {
                            Some(named.clone())
                        };

                        ui.show_status(&format!(
                            "facts · {}",
                            if named.is_empty() {
                                "everything"
                            } else {
                                &named
                            }
                        ))
                        .await?;

                        let facts = self
                            .memory
                            .facts_for(
                                if named.is_empty() {
                                    &task.message
                                } else {
                                    &named
                                },
                                if named.is_empty() {
                                    None
                                } else {
                                    Some(named.as_str())
                                },
                            )
                            .await
                            .unwrap_or_default();

                        results.push(if named.is_empty() {
                            format!(
                                "[FACTS] no longer working on any project in particular.\n{}",
                                crate::memory::facts::render(&facts)
                            )
                        } else {
                            format!(
                                "[FACTS] '{}' is the project you are on now; what is known about \
                                 it is weighted up from here on.\n{}",
                                named,
                                crate::memory::facts::render(&facts)
                            )
                        });
                        continue;
                    }

                    if source == "board" {
                        let Some(group) = &task.group else {
                            results.push(
                                "[ERROR] you are not working in a group, so there is no board"
                                    .to_string(),
                            );
                            continue;
                        };

                        ui.show_status("board").await?;

                        match self.board.entries(&group.id).await {
                            Ok(entries) => results.push(format!(
                                "[BOARD] group {} · {} · {} shared iterations left · with {}\n{}",
                                group.id,
                                first_line(&group.goal),
                                group.iterations_left,
                                if group.members.is_empty() {
                                    "nobody yet".to_string()
                                } else {
                                    group.members.join(", ")
                                },
                                crate::core::board::as_text(
                                    &entries,
                                    &task.agent_id,
                                    task.role.as_deref()
                                )
                            )),
                            Err(e) => results.push(format!("[ERROR] the board: {}", e)),
                        }
                        continue;
                    }

                    if source == "agents" {
                        ui.show_status("agents").await?;

                        let running = self.agents.others(&task.agent_id).await;

                        results.push(format!(
                            "[AGENTS] {}",
                            crate::core::agents::as_text(&running)
                        ));
                        continue;
                    }

                    if source != "memory" {
                        results.push(format!(
                            "[ERROR] unknown data source '{}', only 'memory', 'skill', 'facts', 'agents' and 'board' are supported",
                            source
                        ));
                        continue;
                    }

                    ui.show_status(&format!("memory · {}", query)).await?;
                    let mut hits = self.memory.search(query, *limit).await?;

                    if hits.is_empty()
                        && let Some(widened) = self.widen_query(query).await
                    {
                        ui.show_status(&format!("memory · retry · {}", widened))
                            .await?;
                        hits = self.memory.search(&widened, *limit).await?;
                    }

                    if hits.is_empty() {
                        results.push(format!("[MEMORY] nothing found for '{}'", query));
                    } else {
                        let mut block = format!("[MEMORY] {} result(s):", hits.len());
                        for hit in hits {
                            block.push_str(&format!(
                                "\n  session {} {} [{}] {}",
                                hit.session_id, hit.created_at, hit.role, hit.content
                            ));
                        }
                        results.push(block);
                    }
                }

                ActionType::RequestInboxKey {
                    module,
                    why,
                    skills,
                } => {
                    let text = self.issue_inbox_key(ui, task, module, why, skills).await?;
                    results.push(text);
                }

                ActionType::Remember {
                    subject,
                    key,
                    value,
                    note,
                    owner,
                    scope,
                    scope_ref,
                    pinned,
                    also,
                } => {
                    let wanted = Keeping {
                        subject,
                        key,
                        value,
                        note,
                        owner,
                        scope,
                        scope_ref,
                        pinned: *pinned,
                        also: *also,
                    };
                    let text = self.remember(ui, &wanted).await?;
                    results.push(text);
                }

                ActionType::Forget { subject, key } => {
                    if subject.trim().is_empty() {
                        results.push("[FORGET ERROR] which subject?".to_string());
                        continue;
                    }

                    let key = (!key.trim().is_empty()).then_some(key.as_str());
                    let removed = self.memory.forget(subject, key).await?;

                    ui.show_status(&format!("forget · {}", subject)).await?;
                    results.push(format!(
                        "[FORGOTTEN] {} fact(s) about '{}' removed",
                        removed, subject
                    ));
                }

                ActionType::ScheduleJob {
                    name,
                    task: job_task,
                    schedule,
                    grant,
                } => {
                    let text = self
                        .create_job(ui, task, name, job_task, schedule, grant)
                        .await?;
                    results.push(text);
                }

                ActionType::ManageJobs { operation, id } => {
                    let text = self.manage_jobs(operation, *id).await?;
                    results.push(text);
                }

                ActionType::SpawnAgent {
                    task: subtask,
                    reason,
                    role,
                } => {
                    if subtask.trim().is_empty() {
                        results.push(
                            "[SUBAGENT ERROR] a spawned agent needs a task to work on".to_string(),
                        );
                        continue;
                    }

                    if task.depth >= MAX_DEPTH {
                        results.push(format!(
                            "[SUBAGENT ERROR] already {} levels deep, which is the limit. \
                             Do the work here instead of delegating it again.",
                            task.depth
                        ));
                        continue;
                    }

                    let Some(me) = self.shared() else {
                        results.push(
                            "[SUBAGENT ERROR] this agent cannot spawn another one. \
                             Do the work here."
                                .to_string(),
                        );
                        continue;
                    };

                    ui.show_status(&format!("subagent · {}", first_line(subtask)))
                        .await?;
                    if !reason.trim().is_empty() {
                        ui.show_status(&format!("subagent · because {}", first_line(reason)))
                            .await?;
                    }

                    let wanted = role.trim();
                    if !wanted.is_empty() && !self.role_exists(wanted).await {
                        results.push(format!(
                            "[SUBAGENT ERROR] there is no role called '{}'. Known roles: {}.                              Spawn without a role to get a plain copy of yourself.",
                            wanted,
                            self.role_names().await
                        ));
                        continue;
                    }

                    let group_id = self.group_for(task).await;

                    if task.group.is_none()
                        && let Some(id) = &group_id
                    {
                        self.leave_report(
                            &task.agent_id,
                            format!(
                                "[GROUP] you opened group {} for this work. Everything you spawn \
                                 from here shares it, along with one board and one pot of \
                                 iterations. RequestData source 'board' reads it.",
                                id
                            ),
                        )
                        .await;
                    }
                    let child = self
                        .new_child_task(task, subtask, wanted, group_id.clone())
                        .await;
                    let child_id = child.agent_id.clone();
                    let shown_id = child_id.clone();
                    let parent_task = task.task_id.clone();
                    let errand = first_line(subtask);
                    let errand_for_child = errand.clone();

                    let caller = task.agent_id.clone();
                    let caller_for_report = caller.clone();
                    tokio::spawn(crate::skill_layer::CALLER.scope(caller, async move {
                        let mut detached =
                            crate::core::scheduler::subagent_ui(me.notifier_handle(), &child_id);
                        let started = std::time::Instant::now();
                        let outcome = me.run(&mut detached, child).await;
                        let report = child_report(&errand_for_child, started.elapsed(), &outcome);

                        me.tell(format!("  · {}", first_line(&report)));
                        me.leave_report(&caller_for_report, format!("[SUBAGENT] {}", report))
                            .await;

                        if let Err(e) = me
                            .memory
                            .log(NewMessage::new(Role::System, &report).task(&parent_task))
                            .await
                        {
                            me.tell(format!("  x subagent · its report was not kept: {}", e));
                        }
                    }));

                    results.push(format!(
                        "[SUBAGENT] {}{} is now working on '{}' on its own. Do not wait for it — \
                         carry on, or answer the user. Its report reaches you on a later turn; \
                         RequestData source 'agents' shows what it is doing meanwhile.{}",
                        shown_id,
                        if wanted.is_empty() {
                            String::new()
                        } else {
                            format!(" as {}", wanted)
                        },
                        errand,
                        match &group_id {
                            Some(id) => format!(
                                " You share group {} with it; RequestData source 'board' is where \
                                 you both leave findings.",
                                id
                            ),
                            None => String::new(),
                        }
                    ));
                }

                ActionType::PostToBoard {
                    kind,
                    to,
                    body,
                    entry,
                    state,
                } => {
                    results.push(self.use_board(task, kind, to, body, *entry, state).await);
                }

                ActionType::AskAgent { to, message } => {
                    results.push(self.ask_agent(task, to, message).await);
                }

                ActionType::RequestGrant {
                    skills,
                    new_skills,
                    risky,
                    why,
                    critical,
                } => {
                    let wanted = Grant {
                        skills: skills.clone(),
                        new_skills: *new_skills,
                        risky: *risky,
                    };
                    results.push(self.expand_grant(ui, task, &wanted, why, *critical).await?);
                }

                ActionType::Switch { level, why } => {
                    if !self.levels_enabled().await {
                        results.push(
                            "[SWITCH IGNORED] intelligence levels are not configured on this \
                             machine; there is only one model. Carry on."
                                .to_string(),
                        );
                        continue;
                    }

                    let Some(wanted) = Level::parse(level) else {
                        results.push(format!(
                            "[SWITCH REJECTED] '{}' is not a level. Use low, medium or high.",
                            level
                        ));
                        continue;
                    };

                    let current = self.level().await;

                    if wanted > current && why.trim().is_empty() {
                        results.push(
                            "[SWITCH REJECTED] moving up needs a reason. Say in `why` what about \
                             this task the current level cannot do."
                                .to_string(),
                        );
                        continue;
                    }

                    if wanted < current {
                        results.push(format!(
                            "[SWITCH REFUSED] you are on {} and cannot drop to {} in the middle \
                             of a task. The prompt cache belongs to one model, so coming back \
                             down and up again means paying to re-read this whole context twice. \
                             The next task starts at the default level on its own.",
                            current, wanted
                        ));
                        continue;
                    }

                    if !self.move_to(wanted, Reason::ModelAsked).await {
                        results.push(format!("[SWITCH] already at {}.", current));
                        continue;
                    }

                    let model = self.current_model().await.unwrap_or_default();
                    ui.show_status(&format!("intelligence {} · {}", model, wanted))
                        .await?;

                    self.memory
                        .log(
                            NewMessage::new(
                                Role::System,
                                format!(
                                    "intelligence {} -> {} ({})",
                                    current,
                                    wanted,
                                    if why.trim().is_empty() {
                                        "no reason given"
                                    } else {
                                        why
                                    }
                                ),
                            )
                            .task(&task.task_id),
                        )
                        .await?;

                    results.push(format!(
                        "[SWITCH] now running at {}. The next turn is answered by {}.",
                        wanted, model
                    ));
                }

                ActionType::GenerateChunk {
                    module_name,
                    chunk_index,
                    total_chunks,
                    code_chunk,
                    dependencies,
                    language,
                } => {
                    let Some(language) = Language::parse(language) else {
                        results.push(format!(
                            "[BUILD REJECTED] '{}' is not a language this machine can build. \
                             Use one of: {}. Nothing was buffered; start again from chunk 1.",
                            language,
                            Language::ALL
                                .iter()
                                .map(|l| l.id())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        continue;
                    };

                    self.escalate(ui, Reason::WritingASkill).await?;

                    if self.engine.attempts_for(module_name).await > 1 {
                        self.escalate(ui, Reason::BuildAttempts).await?;
                    }

                    if !self.engine.is_approved(module_name).await {
                        let already_loaded = self.registry.read().await.get(module_name).is_some();

                        let (what, description, risk) = if already_loaded {
                            (
                                format!("rebuild the '{}' skill in {}", module_name, language),
                                format!(
                                    "Replace the existing '{}' skill with newly written {} code. \
                                     The current version is kept as .previous.",
                                    module_name, language
                                ),
                                "high",
                            )
                        } else {
                            (
                                format!("write a new skill '{}' in {}", module_name, language),
                                format!(
                                    "Write, build and install a new skill '{}' in {}. \
                                     The code is written by the model and built on this \
                                     machine; once installed it loads in every future session.",
                                    module_name, language
                                ),
                                "medium",
                            )
                        };

                        let allowed = ui.ask_permission(&what, &description, risk).await?;

                        self.memory
                            .log(
                                NewMessage::new(
                                    Role::System,
                                    format!(
                                        "skill build {} for '{}'",
                                        if allowed { "approved" } else { "refused" },
                                        module_name
                                    ),
                                )
                                .task(&task.task_id),
                            )
                            .await?;

                        if !allowed {
                            return Ok(StepOutcome::Aborted(format!(
                                "[PERMISSION ERROR] refused to build '{}'. Use the skills you \
                                 already have, or explain what is missing.",
                                module_name
                            )));
                        }

                        self.engine.approve(module_name).await;
                    }

                    if chunk_index >= total_chunks {
                        ui.show_status(&format!(
                            "{}: last chunk in, assembling and compiling — this takes a \
                             minute or two, nothing is stuck",
                            module_name
                        ))
                        .await?;
                    }

                    let progress = self
                        .engine
                        .accept_chunk(
                            &self.config.read().await.preflight.clone(),
                            self.config.read().await.agent.max_fix_iterations,
                            Chunk {
                                module: module_name,
                                index: *chunk_index,
                                total: *total_chunks,
                                code: code_chunk,
                                dependencies,
                                language,
                            },
                        )
                        .await?;

                    match progress {
                        Progress::Buffered { received, total } => {
                            ui.show_status(&format!(
                                "{}: chunk {}/{} received",
                                module_name, received, total
                            ))
                            .await?;
                            results.push(format!(
                                "[BUILD] {} — {}/{} chunks buffered, send the rest",
                                module_name, received, total
                            ));
                        }

                        Progress::Rejected(reason) => {
                            results.push(format!("[BUILD ERROR] {}: {}", module_name, reason));
                        }

                        Progress::Built(outcome) => {
                            let text = self.finish_build(ui, task, module_name, outcome).await?;
                            results.push(text);
                        }
                    }
                }
            }
        }

        if response.is_done {
            return Ok(StepOutcome::Finished);
        }

        if results.is_empty() {
            return Ok(StepOutcome::Continue(STALL_CORRECTION.to_string()));
        }

        Ok(StepOutcome::Continue(results.join("\n")))
    }

    async fn issue_inbox_key(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        module: &str,
        why: &str,
        skills: &[String],
    ) -> JumabekResult<String> {
        if task.grant.is_some() {
            return Ok(
                "[NOT GRANTED] a background task cannot open the door for anyone.".to_string(),
            );
        }

        if !crate::core::workshop::is_valid_module_name(module) {
            return Ok(format!(
                "[INBOX ERROR] '{}' is not a skill name — lowercase letters, digits and \
                 underscores, starting with a letter.",
                module
            ));
        }

        if !self.config.read().await.inbox.enabled {
            return Ok(
                "[INBOX ERROR] the inbox is switched off. Ask the user to set [inbox] enabled \
                 = true in config.toml and restart, then try again."
                    .to_string(),
            );
        }

        let listed = if skills.is_empty() {
            "nothing — it can only wake you and pass a message".to_string()
        } else {
            skills.join(", ")
        };

        let allowed = ui
            .ask_permission(
                &format!("let '{}' knock on the inbox", module),
                &format!(
                    "{}\n\nIt will be able to push work in at any time, running as: {}\n\
                     It can never write skills or run commands the safety rules stop.\n\n\
                     A token is generated and written to secrets.toml; the rights go to \
                     config.toml. Nothing is shown to the model.",
                    if why.trim().is_empty() {
                        "No reason given."
                    } else {
                        why
                    },
                    listed
                ),
                "high",
            )
            .await?;

        if !allowed {
            return Ok(format!(
                "[PERMISSION ERROR] the user refused '{}' access to the inbox. Do not ask again \
                 in this conversation.",
                module
            ));
        }

        let config_path = crate::configs::find_file("config.toml")?;
        let secrets_path = crate::configs::find_file("secrets.toml")
            .unwrap_or_else(|_| config_path.with_file_name("secrets.toml"));

        crate::core::inbox::issue::issue(&config_path, &secrets_path, module, skills)?;

        self.memory
            .log(
                NewMessage::new(
                    Role::System,
                    format!("inbox key issued to {} for {}", module, listed),
                )
                .task(&task.task_id),
            )
            .await?;

        ui.show_status(&format!("inbox · {} may now knock", module))
            .await?;

        Ok(format!(
            "[INBOX KEY ISSUED] '{}' has a token and may knock. It reaches the skill within a \
             few seconds, when the changed files are picked up — the skill is restarted then, so \
             do not call it in this same turn. From the skill, read the token from \
             JUMABEK_SKILL_INBOX_TOKEN and POST to http://127.0.0.1:{}/notify with \
             {{\"source\":\"{}\",\"kind\":\"notify\",\"text\":\"...\"}} and an Authorization: \
             Bearer header.",
            module,
            self.config.read().await.inbox.port,
            module
        ))
    }

    async fn remember(
        &self,
        ui: &mut dyn UserInterface,
        wanted: &Keeping<'_>,
    ) -> JumabekResult<String> {
        use crate::memory::facts::{Fact, Scope};

        let mut saved: Vec<String> = Vec::new();
        let mut replaced: Vec<String> = Vec::new();

        if !wanted.subject.trim().is_empty()
            && !wanted.key.trim().is_empty()
            && !wanted.value.trim().is_empty()
        {
            let Some(owner) = crate::memory::facts::owner_of(wanted.owner) else {
                return Ok(format!(
                    "[REMEMBER REFUSED] a fact is either yours or 'shared'; '{}' is neither. \
                     Only the user can put a fact against somebody else.",
                    wanted.owner
                ));
            };

            let Some(scope) = Scope::parse(wanted.scope) else {
                return Ok(format!(
                    "[REMEMBER ERROR] '{}' is not a scope. Use global, language or project.",
                    wanted.scope
                ));
            };

            let mut fact = Fact::new(wanted.subject, wanted.key, wanted.value)
                .owned_by(&owner)
                .about(scope, wanted.scope_ref);
            fact.pinned = wanted.pinned;

            let written = self.memory.remember(&fact, wanted.also).await?;
            replaced = written.replaced;
            saved.push(format!(
                "{} {} = {}",
                wanted.subject, wanted.key, wanted.value
            ));
        }

        if !wanted.note.trim().is_empty() {
            profile::append_note(wanted.note)?;
            saved.push(wanted.note.trim().to_string());
        }

        if saved.is_empty() {
            return Ok(
                "[REMEMBER ERROR] give either subject, key and value together, or a note"
                    .to_string(),
            );
        }

        for line in &saved {
            ui.show_status(&format!("remember · {}", line)).await?;
        }

        let overwritten = if replaced.is_empty() {
            String::new()
        } else {
            format!(
                " It took the place of {} — say so if that was not what you meant.",
                replaced
                    .iter()
                    .map(|old| format!("'{}'", first_line(old)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        Ok(format!(
            "[REMEMBERED] {}.{} This is in front of you from now on — do not tell the user you \
             saved it unless they asked.",
            saved.join("; "),
            overwritten
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_job(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        name: &str,
        job_task: &str,
        schedule: &str,
        grant: &Grant,
    ) -> JumabekResult<String> {
        if task.grant.is_some() {
            return Ok("[NOT GRANTED] a background job cannot create other jobs.".to_string());
        }

        if job_task.trim().is_empty() {
            return Ok("[JOB ERROR] a job needs a task to run".to_string());
        }

        let parsed = match Schedule::parse(schedule) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(format!("[JOB ERROR] {}", e)),
        };

        let name = if name.trim().is_empty() {
            first_line(job_task)
        } else {
            name.to_string()
        };

        let allowed = ui
            .ask_permission(
                &format!("run '{}' in the background", name),
                &format!(
                    "{}\n\nWhen: {}\nIt does: {}\nIt may use: {}\n\n\
                     It runs unattended and will not be able to ask you anything.",
                    name,
                    parsed.describe(),
                    first_line(job_task),
                    grant.describe()
                ),
                if grant.risky || grant.new_skills {
                    "high"
                } else {
                    "medium"
                },
            )
            .await?;

        if !allowed {
            return Ok(format!(
                "[JOB REFUSED] the user did not approve the background job '{}'.",
                name
            ));
        }

        let id = self
            .jobs
            .add(NewJob {
                name: name.clone(),
                task: job_task.to_string(),
                schedule: parsed.clone(),
                grant: grant.clone(),
            })
            .await?;

        self.memory
            .log(
                NewMessage::new(
                    Role::System,
                    format!("background job {} created: {} ({})", id, name, schedule),
                )
                .task(&task.task_id),
            )
            .await?;

        ui.show_status(&format!("job {} · {} · {}", id, name, parsed.describe()))
            .await?;

        Ok(format!(
            "[JOB CREATED] '{}' is job {}, {}. Tell the user its number so they can stop it.",
            name,
            id,
            parsed.describe()
        ))
    }

    async fn manage_jobs(&self, operation: &str, id: i64) -> JumabekResult<String> {
        match operation.trim().to_lowercase().as_str() {
            "list" | "" => {
                let jobs = self.jobs.all().await?;
                if jobs.is_empty() {
                    return Ok("[JOBS] none are scheduled".to_string());
                }

                let mut block = format!("[JOBS] {}:", jobs.len());
                for job in jobs {
                    block.push_str(&format!(
                        "\n  {} [{}] {} — {} — ran {} time(s){}",
                        job.id,
                        job.state.as_str(),
                        job.name,
                        job.schedule.describe(),
                        job.runs,
                        match &job.last_result {
                            Some(last) => format!(", last: {}", first_line(last)),
                            None => String::new(),
                        }
                    ));
                }
                Ok(block)
            }

            "stop" | "cancel" | "remove" | "delete" => {
                if self.jobs.remove(id).await? {
                    Ok(format!("[JOB STOPPED] job {} is gone", id))
                } else {
                    Ok(format!("[JOB ERROR] there is no job {}", id))
                }
            }

            "pause" => Ok(self.switch_job(id, State::Paused, "paused").await?),
            "resume" => Ok(self.switch_job(id, State::Running, "running again").await?),

            other => Ok(format!(
                "[JOB ERROR] unknown operation '{}'. Use list, stop, pause or resume.",
                other
            )),
        }
    }

    async fn switch_job(&self, id: i64, state: State, said: &str) -> JumabekResult<String> {
        if self.jobs.set_state(id, state).await? {
            Ok(format!("[JOB] job {} is {}", id, said))
        } else {
            Ok(format!("[JOB ERROR] there is no job {}", id))
        }
    }

    async fn widen_query(&self, query: &str) -> Option<String> {
        let system = "You expand search queries for a keyword index. Answer with 5 to 12 words only: synonyms and near-synonyms of the query, in the same language as the query plus their English equivalents. Separate them with spaces. No punctuation, no explanation, no quotes.";

        let llm = self.llm.read().await.clone();
        let target = RequestTarget::global(&*self.config.read().await);
        let widened = llm.complete(system, query, &target).await.ok()?;
        let cleaned = clean_expansion(&widened);

        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }

    async fn check_safety(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        module: &str,
        method: &str,
        args: &str,
    ) -> JumabekResult<Option<StepOutcome>> {
        let Some(verdict) = safety::classify(args) else {
            return Ok(None);
        };

        if let Some(grant) = &task.grant
            && !grant.risky
        {
            return Ok(Some(StepOutcome::Aborted(format!(
                "[NOT GRANTED] a safety rule stops this ({}), and this job was not given the \
                 right to override one: {}",
                verdict.reason, args
            ))));
        }

        let allowed = ui
            .ask_permission(
                &format!("{}::{}", module, method),
                &format!(
                    "{}

Blocked by a safety rule: {}.",
                    args, verdict.reason
                ),
                verdict.risk.as_str(),
            )
            .await?;

        self.memory
            .log(
                NewMessage::new(
                    Role::System,
                    format!(
                        "safety gate ({}) {} for: {}",
                        verdict.reason,
                        if allowed { "granted" } else { "denied" },
                        args
                    ),
                )
                .task(&task.task_id),
            )
            .await?;

        if allowed {
            return Ok(None);
        }

        Ok(Some(StepOutcome::Aborted(format!(
            "[PERMISSION ERROR] denied: {} ({}). The task cannot continue without it.",
            args, verdict.reason
        ))))
    }

    async fn run_parallel(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        actions: &[ActionType],
    ) -> JumabekResult<Result<Vec<String>, StepOutcome>> {
        let names: Vec<String> = actions
            .iter()
            .map(|a| match a {
                ActionType::ExecuteModule { module, method, .. } => {
                    format!("{}::{}", module, method)
                }
                other => format!("{:?}", other),
            })
            .collect();

        ui.show_status(&format!("running {} in parallel", names.join(", ")))
            .await?;

        let started = std::time::Instant::now();

        let calls = actions.iter().map(|action| async move {
            let ActionType::ExecuteModule {
                module,
                method,
                args,
                ..
            } = action
            else {
                return (
                    String::new(),
                    String::new(),
                    Err(SkillError::ExecutionFailed(
                        "only module calls can run in parallel".to_string(),
                    )),
                );
            };

            let registry = self.registry.read().await;
            let Some(skill) = registry.get(module) else {
                return (
                    module.clone(),
                    method.clone(),
                    Err(SkillError::NotFound(format!("unknown skill '{}'", module))),
                );
            };

            let outcome = skill.execute(method, args).await;
            (module.clone(), method.clone(), outcome)
        });

        let finished = futures::future::join_all(calls).await;

        ui.show_status(&format!(
            "parallel group done in {:.1}s",
            started.elapsed().as_secs_f64()
        ))
        .await?;

        let mut results = Vec::with_capacity(finished.len());

        for (module, method, outcome) in finished {
            match outcome {
                Ok(output) => {
                    let text = render_output(output);
                    self.memory
                        .log(NewMessage::new(Role::Skill, &text).task(&task.task_id))
                        .await?;
                    results.push(format!("[{}::{}] {}", module, method, text));
                }
                Err(err) => {
                    let wrapped = JumabekError::SkillError(err);
                    self.memory
                        .log(NewMessage::new(Role::Skill, wrapped.to_string()).task(&task.task_id))
                        .await?;

                    if !wrapped.is_recoverable() {
                        return Ok(Err(StepOutcome::Aborted(wrapped.to_string())));
                    }
                    results.push(format!("[{}::{}] {}", module, method, wrapped));
                }
            }
        }

        Ok(Ok(results))
    }

    async fn finish_build(
        &self,
        ui: &mut dyn UserInterface,
        task: &TaskObject,
        module: &str,
        outcome: Outcome,
    ) -> JumabekResult<String> {
        let budget = self.config.read().await.agent.max_fix_iterations;
        let used = self.engine.attempts_for(module).await;
        let left = budget.saturating_sub(used);

        match outcome {
            Outcome::GaveUp {
                attempts,
                last_error,
            } => {
                ui.show_error(&format!(
                    "{}: giving up after {} failed attempt(s)",
                    module, attempts
                ))
                .await?;

                self.memory
                    .log(
                        NewMessage::new(
                            Role::System,
                            format!(
                                "gave up building {} after {} attempts: {}",
                                module, attempts, last_error
                            ),
                        )
                        .task(&task.task_id),
                    )
                    .await?;

                Ok(format!(
                    "[GAVE UP] {} failed {} time(s), which is the limit. Do NOT send more chunks \
                     for it. Solve the task with the skills you already have, or tell the user \
                     what is missing. Last error:
{}",
                    module, attempts, last_error
                ))
            }

            Outcome::CompileFailed(stderr) => {
                ui.show_status(&format!("{}: does not compile", module))
                    .await?;
                self.memory
                    .log(
                        NewMessage::new(Role::System, format!("build failed for {}", module))
                            .task(&task.task_id),
                    )
                    .await?;
                Ok(format!(
                    "[BUILD FAILED] {} did not compile ({} attempt(s) left). Fix the code and \
                     resend every chunk.
{}",
                    module, left, stderr
                ))
            }

            Outcome::ValidationFailed(report) => {
                ui.show_status(&format!("{}: rejected by the validator", module))
                    .await?;
                self.memory
                    .log(
                        NewMessage::new(Role::System, format!("validator rejected {}", module))
                            .task(&task.task_id),
                    )
                    .await?;
                Ok(format!(
                    "[VALIDATOR REJECTED] {} compiled but failed its checks ({} attempt(s) left). \
                     Fix and resend.
{}",
                    module, left, report
                ))
            }

            Outcome::ToolchainMissing { language, detail } => {
                ui.show_error(&format!(
                    "{}: cannot build {} here — {}",
                    module, language, detail
                ))
                .await?;
                Ok(format!(
                    "[TOOLCHAIN MISSING] {} was not built: {}. Rewriting the code will not help \
                     and this did not cost an attempt. Either write the skill in a language this \
                     machine already has, or tell the user what to install.",
                    module, detail
                ))
            }

            Outcome::PreflightUnavailable(detail) => {
                ui.show_error(&format!(
                    "{}: cannot build without a preflight container — {}",
                    module, detail
                ))
                .await?;
                Ok(format!(
                    "[PREFLIGHT UNAVAILABLE] {} was not built: {}. Start Docker Desktop, or set \
                     allow_without_docker = true in [preflight] to build without the check.",
                    module, detail
                ))
            }

            Outcome::Deployed {
                path,
                report,
                preflight,
            } => {
                ui.show_status(&format!("{}: preflight {}", module, preflight))
                    .await?;
                ui.show_status(&format!("{}: built and validated", module))
                    .await?;

                let settings = self.config.read().await.settings_for_skill(module);
                let loaded = match SkillRpcClient::spawn_with_settings(&path, settings).await {
                    Ok(client) => {
                        let methods: Vec<String> = client
                            .methods_cached()
                            .iter()
                            .map(|m| m.method.clone())
                            .collect();
                        self.registry
                            .write()
                            .await
                            .register(Box::new(client) as Box<dyn SkillModule>);
                        ui.show_status(&format!("{} is live: {}", module, methods.join(", ")))
                            .await?;
                        Some(methods)
                    }
                    Err(e) => {
                        ui.show_error(&format!("{} built but could not be loaded: {}", module, e))
                            .await?;
                        None
                    }
                };

                self.memory
                    .log(
                        NewMessage::new(
                            Role::System,
                            format!("deployed skill {} to {}", module, path.display()),
                        )
                        .task(&task.task_id),
                    )
                    .await?;

                Ok(match loaded {
                    Some(methods) => format!(
                        "[BUILT] {} passed every check and is loaded right now. \
                         Methods: {}. You can call it immediately.
{}",
                        module,
                        methods.join(", "),
                        report
                    ),
                    None => format!(
                        "[BUILT] {} passed validation and was saved, but could not be loaded into \
                         this session. It will be available after a restart.",
                        module
                    ),
                })
            }
        }
    }

    async fn available_names(&self) -> String {
        let registry = self.registry.read().await;
        let names: Vec<String> = registry
            .list()
            .into_iter()
            .map(|m| m.name.clone())
            .collect();

        if names.is_empty() {
            "<none>".to_string()
        } else {
            names.join(", ")
        }
    }
}

fn stage_actions(stage: &planner::Stage) -> Vec<&ActionType> {
    match stage {
        planner::Stage::Single(action) => vec![action],
        planner::Stage::Parallel(list) => list.iter().collect(),
    }
}

fn refuse_outside_grant(task: &TaskObject, action: &ActionType) -> Option<String> {
    let grant = task.grant.as_ref()?;

    match action {
        ActionType::ExecuteModule { module, .. } if !grant.allows(module) => Some(format!(
            "[NOT GRANTED] this job may use {} and nothing else, but it tried to call '{}'. \
             Finish with what you have, or report that the job needs wider rights.",
            if grant.skills.is_empty() {
                "no skills".to_string()
            } else {
                grant.skills.join(", ")
            },
            module
        )),

        ActionType::GenerateChunk { module_name, .. } if !grant.new_skills => Some(format!(
            "[NOT GRANTED] this job was not allowed to write new skills, so '{}' cannot be \
             built here. Report what is missing instead.",
            module_name
        )),

        ActionType::PermissionRequest { action, .. } => Some(format!(
            "[NO ONE TO ASK] this job runs in the background and cannot ask about '{}'. \
             What a job may do is fixed when it is created.",
            action
        )),

        ActionType::PromptToUser { .. } => Some(
            "[NO ONE TO ASK] this job runs in the background with nobody at the prompt. \
             Finish with what you already know, or report what you needed to ask."
                .to_string(),
        ),

        _ => None,
    }
}

fn reads_the_whole_session(task: &TaskObject) -> bool {
    task.parent_task_id.is_none() && task.grant.is_none()
}

fn own_messages(
    history: Vec<crate::memory::StoredMessage>,
    task_id: &str,
) -> Vec<crate::memory::StoredMessage> {
    history
        .into_iter()
        .filter(|m| m.task_id.as_deref() == Some(task_id))
        .collect()
}

fn queue_report(waiting: &mut Vec<Letter>, letter: Letter) {
    if waiting.len() >= REPORTS_KEPT {
        waiting.remove(0);
    }
    waiting.push(letter);
}

fn drain_reports(waiting: &mut Vec<Letter>, for_agent: &str) -> Vec<Letter> {
    let mut mine = Vec::new();
    waiting.retain(|letter| {
        if letter.to == for_agent {
            mine.push(letter.clone());
            false
        } else {
            true
        }
    });
    mine
}

fn apply_letters(task: &mut TaskObject, delivered: &[Letter]) {
    for letter in delivered {
        if let Some(tighter) = &letter.narrow {
            task.grant = Some(match &task.grant {
                Some(current) => current.narrow(tighter),
                None => tighter.clone(),
            });
        }

        if let Some(wider) = &letter.widen {
            task.grant = Some(match &task.grant {
                Some(current) => widen(current, wider),
                None => wider.clone(),
            });
        }
    }
}

fn widen(current: &Grant, extra: &Grant) -> Grant {
    let mut skills = current.skills.clone();
    for skill in &extra.skills {
        if !skills.iter().any(|have| have == skill) {
            skills.push(skill.clone());
        }
    }

    Grant {
        skills,
        new_skills: current.new_skills || extra.new_skills,
        risky: current.risky || extra.risky,
    }
}

fn join_reports(existing: Option<String>, delivered: Vec<String>) -> String {
    let arrived = delivered.join("\n");
    match existing {
        Some(text) if !text.trim().is_empty() => format!("{}\n{}", text, arrived),
        _ => arrived,
    }
}

fn child_report(
    errand: &str,
    took: std::time::Duration,
    outcome: &JumabekResult<String>,
) -> String {
    match outcome {
        Ok(summary) => format!(
            "the agent you spawned for '{}' finished in {:.0}s and reported: {}",
            errand,
            took.as_secs_f64(),
            summary.trim()
        ),
        Err(e) => format!(
            "the agent you spawned for '{}' died after {:.0}s without finishing: {}",
            errand,
            took.as_secs_f64(),
            e
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    NotUnderGrant,
    NothingAsked,
    AboveCeiling,
    PutToTheUser,
    SentUpward,
    GrantedByMainAgent,
}

impl Verdict {
    fn recorded(self) -> Option<(&'static str, &'static str)> {
        match self {
            Verdict::NotUnderGrant | Verdict::NothingAsked => None,
            Verdict::AboveCeiling => Some(("refused above the ceiling", "config")),
            Verdict::SentUpward => Some(("left for the user", "queued")),
            Verdict::GrantedByMainAgent => Some(("granted", "main agent")),
            Verdict::PutToTheUser => None,
        }
    }

    #[cfg(test)]
    fn decides_anything(self) -> bool {
        !matches!(self, Verdict::NotUnderGrant | Verdict::NothingAsked)
    }
}

fn decide_grant(
    under: Option<&Grant>,
    wanted: &Grant,
    ceiling: &Grant,
    critical: bool,
    someone_present: bool,
) -> Verdict {
    if under.is_none() {
        return Verdict::NotUnderGrant;
    }

    if wanted.skills.is_empty() && !wanted.new_skills && !wanted.risky {
        return Verdict::NothingAsked;
    }

    if !wanted.within(ceiling) {
        return Verdict::AboveCeiling;
    }

    match (critical, someone_present) {
        (true, true) => Verdict::PutToTheUser,
        (true, false) => Verdict::SentUpward,
        (false, _) => Verdict::GrantedByMainAgent,
    }
}

struct Keeping<'a> {
    subject: &'a str,
    key: &'a str,
    value: &'a str,
    note: &'a str,
    owner: &'a str,
    scope: &'a str,
    scope_ref: &'a str,
    pinned: bool,
    also: bool,
}

fn discussion_key(group: &str, a: &str, b: &str) -> String {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("{}|{}|{}", group, first, second)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect()
}

fn clean_expansion(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| word.chars().count() >= 2)
        .take(16)
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_for_index(text: &str) -> String {
    match text.char_indices().nth(INDEXED_CONTENT_LIMIT) {
        Some((idx, _)) => format!(
            "{}… [{} more characters kept in raw_json]",
            &text[..idx],
            text.chars().count() - INDEXED_CONTENT_LIMIT
        ),
        None => text.to_string(),
    }
}

fn render_output(output: SkillOutput) -> String {
    match output {
        SkillOutput::Text(text) => text,
        SkillOutput::Json(value) => value.to_string(),
        SkillOutput::Binary(bytes) => format!("<{} bytes of binary data>", bytes.len()),
        SkillOutput::Empty => "<no output>".to_string(),
    }
}

fn system_info() -> SystemInfo {
    SystemInfo {
        os: format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
        shell: if cfg!(windows) {
            "powershell".to_string()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
        },
        current_time: Local::now().to_rfc3339(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        jumabek_home: crate::configs::jumabek_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod subagent_tests {
    use super::*;

    fn took() -> std::time::Duration {
        std::time::Duration::from_secs(3)
    }

    #[test]
    fn a_child_that_finished_reports_its_summary_and_nothing_else() {
        let report = child_report(
            "count the log files",
            took(),
            &Ok("  there are 41, of which 3 are empty  ".to_string()),
        );

        assert!(report.contains("count the log files"), "{report}");
        assert!(
            report.contains("there are 41, of which 3 are empty"),
            "{report}"
        );
        assert!(report.contains("3s"), "{report}");
    }

    #[test]
    fn a_child_that_died_says_so_instead_of_leaving_the_parent_waiting() {
        let report = child_report(
            "count the log files",
            took(),
            &Err(JumabekError::InternalError(
                "the skill went away".to_string(),
            )),
        );

        assert!(report.contains("without finishing"), "{report}");
        assert!(report.contains("the skill went away"), "{report}");
        assert!(
            !report.contains("reported:"),
            "a death was dressed up as a result: {report}"
        );
    }

    #[test]
    fn a_report_reaches_the_agent_that_spawned_the_child_and_nobody_else() {
        let mut waiting = Vec::new();
        queue_report(
            &mut waiting,
            Letter::plain("parent", "[SUBAGENT] mine".to_string()),
        );
        queue_report(
            &mut waiting,
            Letter::plain("stranger", "[SUBAGENT] not yours".to_string()),
        );

        let mine = drain_reports(&mut waiting, "parent");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].text, "[SUBAGENT] mine");
        assert_eq!(
            waiting.len(),
            1,
            "another agent's report was taken along with it"
        );
    }

    #[test]
    fn a_report_is_delivered_once_and_not_again_on_every_later_turn() {
        let mut waiting = Vec::new();
        queue_report(
            &mut waiting,
            Letter::plain("parent", "[SUBAGENT] done".to_string()),
        );

        assert_eq!(drain_reports(&mut waiting, "parent").len(), 1);
        assert!(
            drain_reports(&mut waiting, "parent").is_empty(),
            "the same report arrived twice"
        );
    }

    #[test]
    fn an_agent_that_never_comes_back_cannot_grow_the_queue_without_end() {
        let mut waiting = Vec::new();
        for n in 0..REPORTS_KEPT + 5 {
            queue_report(&mut waiting, Letter::plain("gone", format!("report {n}")));
        }

        assert_eq!(waiting.len(), REPORTS_KEPT);
        assert_eq!(
            waiting.last().map(|letter| letter.text.as_str()),
            Some(format!("report {}", REPORTS_KEPT + 4).as_str()),
            "the queue dropped the newest instead of the oldest"
        );
    }

    #[test]
    fn a_report_is_added_to_whatever_the_turn_already_had_to_say() {
        let joined = join_reports(
            Some("[SHELL] 41 files".to_string()),
            vec!["[SUBAGENT] the other one finished".to_string()],
        );

        assert_eq!(
            joined,
            "[SHELL] 41 files\n[SUBAGENT] the other one finished"
        );
    }

    #[test]
    fn a_report_arriving_into_an_empty_turn_stands_on_its_own() {
        assert_eq!(
            join_reports(None, vec!["[SUBAGENT] done".to_string()]),
            "[SUBAGENT] done"
        );
        assert_eq!(
            join_reports(Some("   ".to_string()), vec!["[SUBAGENT] done".to_string()]),
            "[SUBAGENT] done"
        );
    }

    #[test]
    fn several_children_finishing_at_once_all_get_through() {
        let joined = join_reports(
            None,
            vec!["[SUBAGENT] one".to_string(), "[SUBAGENT] two".to_string()],
        );

        assert_eq!(
            joined.lines().count(),
            2,
            "a report was swallowed: {joined}"
        );
    }

    fn grant(skills: &[&str], new_skills: bool, risky: bool) -> Grant {
        Grant {
            skills: skills.iter().map(|s| s.to_string()).collect(),
            new_skills,
            risky,
        }
    }

    fn task_with(grant: Option<Grant>) -> TaskObject {
        TaskObject {
            task_id: "t".to_string(),
            agent_id: "a".to_string(),
            parent_task_id: None,
            message: String::new(),
            system_info: system_info(),
            system_response: None,
            skills: Vec::new(),
            capabilities: Vec::new(),
            constraints: Constraints {
                max_iterations: 10,
                max_fix_iterations: 5,
            },
            iteration: 0,
            fix_iteration: 0,
            depth: 0,
            role: None,
            group: None,
            grant,
            origin: None,
            intelligence: None,
            interface_mode: InterfaceMode::Cli,
        }
    }

    #[test]
    fn a_sideways_request_narrows_the_agent_that_answers_it() {
        let mut task = task_with(Some(grant(&["shell_executor", "telegram"], true, true)));

        apply_letters(
            &mut task,
            &[Letter {
                to: "a".to_string(),
                text: "[FROM abc] have a look".to_string(),
                narrow: Some(grant(&["telegram"], false, false)),
                widen: None,
            }],
        );

        let now = task.grant.expect("the grant went missing");
        assert_eq!(now.skills, vec!["telegram"]);
        assert!(!now.new_skills, "the asker's answer kept a right it lacked");
        assert!(!now.risky);
    }

    #[test]
    fn a_granted_right_arrives_on_the_next_turn_and_adds_to_what_was_there() {
        let mut task = task_with(Some(grant(&["telegram"], false, false)));

        apply_letters(
            &mut task,
            &[Letter {
                to: "a".to_string(),
                text: "[RIGHTS] widened".to_string(),
                narrow: None,
                widen: Some(grant(&["shell_executor"], false, false)),
            }],
        );

        let now = task.grant.expect("the grant went missing");
        assert!(now.allows("telegram"), "the old right was thrown away");
        assert!(now.allows("shell_executor"), "the new right never landed");
    }

    #[test]
    fn a_narrowing_that_lands_after_a_widening_still_wins() {
        let mut task = task_with(Some(grant(&["telegram"], false, false)));

        apply_letters(
            &mut task,
            &[
                Letter {
                    to: "a".to_string(),
                    text: "wider".to_string(),
                    narrow: None,
                    widen: Some(grant(&["shell_executor"], true, true)),
                },
                Letter {
                    to: "a".to_string(),
                    text: "on behalf of someone smaller".to_string(),
                    narrow: Some(grant(&["telegram"], false, false)),
                    widen: None,
                },
            ],
        );

        let now = task.grant.expect("the grant went missing");
        assert_eq!(now.skills, vec!["telegram"]);
        assert!(!now.risky);
    }

    fn ceiling() -> Grant {
        grant(&["*"], false, false)
    }

    #[test]
    fn an_agent_under_no_grant_is_told_there_is_nothing_to_widen() {
        let verdict = decide_grant(None, &grant(&["x"], false, false), &ceiling(), false, true);
        assert_eq!(verdict, Verdict::NotUnderGrant);
        assert!(!verdict.decides_anything());
    }

    #[test]
    fn asking_for_nothing_in_particular_decides_nothing() {
        let verdict = decide_grant(
            Some(&grant(&["a"], false, false)),
            &grant(&[], false, false),
            &ceiling(),
            false,
            true,
        );
        assert_eq!(verdict, Verdict::NothingAsked);
    }

    #[test]
    fn an_ordinary_request_inside_the_ceiling_is_settled_by_the_main_agent() {
        assert_eq!(
            decide_grant(
                Some(&grant(&["a"], false, false)),
                &grant(&["searxng_search"], false, false),
                &ceiling(),
                false,
                true,
            ),
            Verdict::GrantedByMainAgent
        );
    }

    #[test]
    fn a_critical_request_waits_for_the_user_when_the_user_is_there() {
        assert_eq!(
            decide_grant(
                Some(&grant(&["a"], false, false)),
                &grant(&["searxng_search"], false, false),
                &ceiling(),
                true,
                true,
            ),
            Verdict::PutToTheUser
        );
    }

    #[test]
    fn a_critical_request_with_nobody_at_the_keyboard_goes_up_instead_of_waiting() {
        assert_eq!(
            decide_grant(
                Some(&grant(&["a"], false, false)),
                &grant(&["searxng_search"], false, false),
                &ceiling(),
                true,
                false,
            ),
            Verdict::SentUpward
        );
    }

    #[test]
    fn the_ceiling_refuses_whoever_is_asking_and_whoever_would_answer() {
        for (critical, present) in [(false, true), (true, true), (true, false)] {
            assert_eq!(
                decide_grant(
                    Some(&grant(&["a"], false, false)),
                    &grant(&[], true, false),
                    &ceiling(),
                    critical,
                    present,
                ),
                Verdict::AboveCeiling,
                "the ceiling was got round with critical={critical} present={present}"
            );
        }
    }

    #[test]
    fn every_decision_leaves_a_record_of_who_made_it() {
        for verdict in [
            Verdict::NotUnderGrant,
            Verdict::NothingAsked,
            Verdict::AboveCeiling,
            Verdict::PutToTheUser,
            Verdict::SentUpward,
            Verdict::GrantedByMainAgent,
        ] {
            if !verdict.decides_anything() {
                assert!(verdict.recorded().is_none(), "{verdict:?}");
                continue;
            }

            if verdict == Verdict::PutToTheUser {
                continue;
            }

            let (outcome, by) = verdict
                .recorded()
                .unwrap_or_else(|| panic!("{verdict:?} decided something and wrote nothing down"));
            assert!(!outcome.is_empty() && !by.is_empty(), "{verdict:?}");
        }
    }

    #[test]
    fn two_agents_talking_are_counted_the_same_way_round_either_way() {
        assert_eq!(
            discussion_key("g", "alice", "bob"),
            discussion_key("g", "bob", "alice"),
            "a pair could double its turns by swapping who asks"
        );
        assert_ne!(
            discussion_key("g1", "alice", "bob"),
            discussion_key("g2", "alice", "bob"),
            "two groups shared one discussion budget"
        );
    }

    #[test]
    fn the_depth_limit_still_refuses_a_third_level() {
        assert_eq!(
            MAX_DEPTH, 2,
            "the nesting limit moved without being decided"
        );
    }

    #[test]
    fn a_spawned_agent_is_told_it_may_not_ask_a_question() {
        let mut ui = crate::core::scheduler::detached_ui();

        let allowed = tokio_test_block(ui.ask_permission("rm -rf /", "", "high"));
        assert!(!allowed, "a detached child granted itself permission");
    }

    fn tokio_test_block<F: std::future::Future<Output = JumabekResult<bool>>>(future: F) -> bool {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
            .expect("the detached ui should refuse, not fail")
    }
}

#[cfg(test)]
mod expansion_tests {
    use super::*;
    use jumabek_sdk::{MethodInfo, ModuleMetadata, SkillError, SkillOutput};

    struct Stub {
        metadata: ModuleMetadata,
        bulk: usize,
    }

    impl Stub {
        fn new(name: &str) -> Self {
            Stub::sized(name, 0)
        }

        fn sized(name: &str, bulk: usize) -> Self {
            Stub {
                metadata: ModuleMetadata {
                    name: name.to_string(),
                    version: "1.0.0".to_string(),
                    description: "a stub".to_string(),
                },
                bulk,
            }
        }
    }

    #[async_trait::async_trait]
    impl SkillModule for Stub {
        fn get_metadata(&self) -> &ModuleMetadata {
            &self.metadata
        }
        fn health_check(&self) -> bool {
            true
        }
        fn available_methods(&self) -> Vec<MethodInfo> {
            vec![MethodInfo {
                method: "run".to_string(),
                description: "runs".to_string(),
                args_description: "a".repeat(self.bulk.max(4)),
            }]
        }
        async fn execute(&self, _: &str, _: &str) -> Result<SkillOutput, SkillError> {
            Ok(SkillOutput::Empty)
        }
    }

    fn registry_of(count: usize) -> SkillRegistry {
        let mut registry = SkillRegistry::new();
        for i in 0..count {
            registry.register(Box::new(Stub::new(&format!("skill{}", i))) as Box<dyn SkillModule>);
        }
        registry
    }

    fn describe(registry: SkillRegistry, expanded: &[&str]) -> Vec<TaskObjectSkill> {
        let asked: std::collections::HashSet<String> =
            expanded.iter().map(|s| s.to_string()).collect();
        let affordable = within_budget(&registry);

        registry
            .all()
            .map(|skill| {
                let metadata = skill.get_metadata();
                let show = affordable.contains(&metadata.name) || asked.contains(&metadata.name);
                TaskObjectSkill {
                    name: metadata.name.clone(),
                    description: metadata.description.clone(),
                    available_methods: if show {
                        skill
                            .available_methods()
                            .into_iter()
                            .map(|m| TaskObjectSkillMethod {
                                method: m.method,
                                description: m.description,
                                args_description: m.args_description,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect()
    }

    fn stored(id: i64, task_id: Option<&str>) -> crate::memory::StoredMessage {
        crate::memory::StoredMessage {
            id,
            task_id: task_id.map(|t| t.to_string()),
            role: "user".to_string(),
            content: format!("message {}", id),
            raw_json: None,
        }
    }

    fn task_with(parent: Option<&str>, grant: Option<Grant>) -> TaskObject {
        TaskObject {
            task_id: "t".to_string(),
            agent_id: "a".to_string(),
            parent_task_id: parent.map(|p| p.to_string()),
            message: String::new(),
            system_info: system_info(),
            system_response: None,
            skills: Vec::new(),
            capabilities: Vec::new(),
            constraints: Constraints {
                max_iterations: 10,
                max_fix_iterations: 5,
            },
            iteration: 0,
            fix_iteration: 0,
            depth: 0,
            role: None,
            group: None,
            grant,
            origin: None,
            intelligence: None,
            interface_mode: InterfaceMode::Cli,
        }
    }

    #[test]
    fn a_background_job_does_not_read_the_conversation() {
        let job = task_with(None, Some(Grant::default()));
        assert!(
            !reads_the_whole_session(&job),
            "a job running unattended was handed the whole interactive session"
        );
    }

    #[test]
    fn an_ordinary_request_still_reads_the_session() {
        assert!(reads_the_whole_session(&task_with(None, None)));
    }

    #[test]
    fn a_subagent_never_reads_the_session() {
        assert!(!reads_the_whole_session(&task_with(Some("parent"), None)));
    }

    #[test]
    fn a_subagent_reads_only_its_own_exchanges() {
        let history = vec![
            stored(1, Some("parent")),
            stored(2, Some("child")),
            stored(3, None),
            stored(4, Some("child")),
            stored(5, Some("other-child")),
        ];

        let kept: Vec<i64> = own_messages(history, "child")
            .iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(
            kept,
            vec![2, 4],
            "a subagent was handed messages that are not its own"
        );
    }

    #[test]
    fn a_root_task_keeps_the_whole_session() {
        let history = vec![stored(1, Some("a")), stored(2, None)];
        assert_eq!(own_messages(history, "a").len(), 1);
    }

    #[test]
    fn a_parse_failure_is_summarised_to_one_line() {
        let detail = format!("cannot read the answer: {}\nsecond line", "x".repeat(300));
        let summary = first_line(&detail);

        assert!(!summary.contains('\n'), "status line spans lines");
        assert!(summary.chars().count() <= 120);
    }

    #[test]
    fn model_facing_text_has_no_collapsed_line_breaks() {
        assert!(
            !PARSE_CORRECTION.contains("   "),
            "PARSE_CORRECTION lost a line continuation"
        );
    }

    #[test]
    fn skills_that_fit_the_budget_are_sent_in_full() {
        let described = describe(registry_of(3), &[]);
        assert!(
            described.iter().all(|s| !s.available_methods.is_empty()),
            "methods were withheld when there was room for them"
        );
    }

    #[test]
    fn one_verbose_skill_does_not_crowd_out_the_rest() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(Stub::sized("chatty", SKILL_METHOD_BUDGET + 1)));
        for i in 0..3 {
            registry.register(Box::new(Stub::new(&format!("small{}", i))));
        }

        let described = describe(registry, &[]);
        let withheld: Vec<&str> = described
            .iter()
            .filter(|s| s.available_methods.is_empty())
            .map(|s| s.name.as_str())
            .collect();

        assert_eq!(
            withheld,
            vec!["chatty"],
            "the wrong skills were collapsed: counting skills is not measuring them"
        );
        assert!(
            described.iter().all(|s| !s.description.is_empty()),
            "a skill lost its summary and became unfindable"
        );
    }

    #[test]
    fn a_skill_the_model_asked_for_is_sent_however_big_it_is() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(Stub::sized("chatty", SKILL_METHOD_BUDGET + 1)));

        let described = describe(registry, &["chatty"]);
        assert!(
            !described[0].available_methods.is_empty(),
            "the model asked for this one and was refused"
        );
    }

    #[test]
    fn the_budget_is_never_exceeded_by_what_rides_along_uninvited() {
        let mut registry = SkillRegistry::new();
        for i in 0..12 {
            registry.register(Box::new(Stub::sized(&format!("skill{:02}", i), 400)));
        }

        let spent: usize = describe(registry, &[])
            .iter()
            .flat_map(|s| &s.available_methods)
            .map(|m| m.method.len() + m.description.len() + m.args_description.len())
            .sum();

        assert!(spent <= SKILL_METHOD_BUDGET, "sent {spent} chars uninvited");
    }

    fn execute(module: &str, method: &str, args: &str) -> ActionType {
        ActionType::ExecuteModule {
            module: module.to_string(),
            method: method.to_string(),
            args: args.to_string(),
            parallel: false,
        }
    }

    fn response_with(actions: Vec<ActionType>) -> AgentResponse {
        AgentResponse {
            message: String::new(),
            is_done: false,
            actions,
        }
    }

    #[test]
    fn a_plain_answer_has_no_fingerprint() {
        assert!(execute_fingerprint(&response_with(vec![ActionType::RespondToUser])).is_none());
    }

    #[test]
    fn a_finished_response_with_no_actions_has_no_fingerprint() {
        let finished = AgentResponse {
            message: String::new(),
            is_done: true,
            actions: Vec::new(),
        };
        assert!(execute_fingerprint(&finished).is_none());
    }

    #[test]
    fn an_unfinished_response_with_no_actions_is_a_stall() {
        assert_eq!(
            execute_fingerprint(&response_with(Vec::new())),
            Some(STALL_FINGERPRINT.to_string()),
            "is_done: false with no actions is not silence, it is stalling"
        );
    }

    #[test]
    fn a_stalled_turn_is_not_shown_to_the_user() {
        let stalled = AgentResponse {
            message: "Let me check that for you.".to_string(),
            is_done: false,
            actions: Vec::new(),
        };
        assert!(
            is_stall(&stalled),
            "a turn with no actions and is_done: false is the one the core corrects"
        );

        let answered = AgentResponse {
            message: "Let me check that for you.".to_string(),
            is_done: true,
            actions: Vec::new(),
        };
        let working = AgentResponse {
            message: "Let me check that for you.".to_string(),
            is_done: false,
            actions: vec![execute("shell_executor", "execute_command", "ls")],
        };
        assert!(!is_stall(&answered), "a finished answer was withheld");
        assert!(
            !is_stall(&working),
            "a turn that is doing work was withheld"
        );
    }

    #[test]
    fn identical_calls_fingerprint_the_same_regardless_of_order() {
        let a = response_with(vec![
            execute("shell_executor", "run", "ls"),
            execute("weather", "today", "Almaty"),
        ]);
        let b = response_with(vec![
            execute("weather", "today", "Almaty"),
            execute("shell_executor", "run", "ls"),
        ]);

        assert_eq!(execute_fingerprint(&a), execute_fingerprint(&b));
    }

    #[test]
    fn a_different_call_changes_the_fingerprint() {
        let a = response_with(vec![execute("shell_executor", "run", "ls")]);
        let b = response_with(vec![execute("shell_executor", "run", "pwd")]);

        assert_ne!(execute_fingerprint(&a), execute_fingerprint(&b));
    }

    #[test]
    fn three_identical_turns_are_circling() {
        let mut recent = std::collections::VecDeque::new();
        recent.push_back("shell_executor::run::ls".to_string());
        recent.push_back("shell_executor::run::ls".to_string());
        assert!(!is_circling(&recent), "two repeats is not yet circling");

        recent.push_back("shell_executor::run::ls".to_string());
        assert!(
            is_circling(&recent),
            "three identical turns should be circling"
        );
    }

    #[test]
    fn three_different_turns_are_not_circling() {
        let mut recent = std::collections::VecDeque::new();
        recent.push_back("a".to_string());
        recent.push_back("b".to_string());
        recent.push_back("c".to_string());
        assert!(
            !is_circling(&recent),
            "different steps were mistaken for being stuck"
        );
    }
}
