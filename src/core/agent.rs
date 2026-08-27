use chrono::Local;
use jumabek_sdk::{SkillError, SkillOutput};

use crate::configs::Config;
use crate::core::context::ContextBuilder;
use crate::core::intelligence::{Level, Reason, Standing};
use crate::core::jobs::{JobStore, NewJob, Schedule, State};
use crate::core::languages::Language;
use crate::core::llm::{LlmClient, RequestTarget};
use crate::core::planner;
use crate::core::profile;
use crate::core::safety;
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
use tokio::sync::RwLock;

const INDEXED_CONTENT_LIMIT: usize = 2_000;

const SKILL_METHOD_BUDGET: usize = 2_000;

const PARSE_RETRIES: u32 = 2;

const PARSE_CORRECTION: &str = "Your previous answer could not be read as an agent response and \
     was discarded. Answer the same request again. Reply with one JSON object and nothing else: no \
     prose before or after it, no markdown fence. If the last answer was cut off, make this one \
     shorter.";

const STALL_CORRECTION: &str = "Your last answer said is_done: false but sent no actions, so \
     nothing actually ran — a message like \"one moment\" or \"checking now\" is not itself an \
     action. Either send a real action (ExecuteModule, PromptToUser, SpawnAgent, ...) this turn, \
     or set is_done: true if you are actually finished.";

const CAPABILITIES: [&str; 13] = [
    "ExecuteModule",
    "PermissionRequest",
    "PromptToUser",
    "RequestData",
    "Remember",
    "Forget",
    "RequestInboxKey",
    "SpawnAgent",
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
    ) -> JumabekResult<Self> {
        let llm = LlmClient::new(&config)?;
        let context =
            ContextBuilder::new(config.system_prompt.clone(), config.llm.context_token_limit);
        let jobs = JobStore::open(&config.db_path())?;
        let starting = config.llm.intelligence.starting_level();
        let starting_model = model_for(&config, starting);

        Ok(Agent {
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
        })
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
        job_task.grant = Some(grant);
        job_task.origin = Some(origin);
        self.run(&mut detached, job_task).await
    }

    pub async fn run_job(
        &self,
        ui: &mut dyn UserInterface,
        task: String,
        grant: Grant,
    ) -> JumabekResult<String> {
        let mut job_task = self.new_task(&uuid::Uuid::new_v4().to_string(), task).await;
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
        Box::pin(self.run_loop(ui, task))
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

            let history = self.history_for(&task).await?;
            let profile = self.profile_block().await;
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

    async fn profile_block(&self) -> String {
        let facts = self.memory.known_facts().await.unwrap_or_default();
        profile::block(&facts, &profile::read_notes())
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
                sent.push(crate::core::task::LlmMessage {
                    role: "system".to_string(),
                    content: PARSE_CORRECTION.to_string(),
                });
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
            grant: None,
            origin: None,
            intelligence: None,
            interface_mode: *self.mode.read().await,
        }
    }

    async fn new_child_task(&self, parent: &TaskObject, request: &str) -> TaskObject {
        let mut child = self
            .new_task(&uuid::Uuid::new_v4().to_string(), request.to_string())
            .await;
        child.parent_task_id = Some(parent.task_id.clone());
        child.depth = parent.depth + 1;
        child.grant = parent.grant.clone();
        child
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
                match self.run_parallel(ui, task, list).await? {
                    Ok(mut batch) => results.append(&mut batch),
                    Err(outcome) => return Ok(outcome),
                }
                continue;
            }

            let action = stage_actions(stage)[0];
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
                    let allowed = ui.ask_permission(action, description, risk_level).await?;
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

                    if source != "memory" {
                        results.push(format!(
                            "[ERROR] unknown data source '{}', only 'memory' and 'skill' are supported",
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
                } => {
                    let text = self.remember(ui, subject, key, value, note).await?;
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

                    ui.show_status(&format!("subagent · {}", first_line(subtask)))
                        .await?;
                    if !reason.trim().is_empty() {
                        ui.show_status(&format!("subagent · because {}", first_line(reason)))
                            .await?;
                    }

                    let child = self.new_child_task(task, subtask).await;
                    let child_id = child.task_id.clone();
                    let started = std::time::Instant::now();
                    let summary = self.run(ui, child).await?;

                    ui.show_status(&format!(
                        "subagent · done in {:.1}s",
                        started.elapsed().as_secs_f64()
                    ))
                    .await?;

                    self.memory
                        .log(
                            NewMessage::new(
                                Role::System,
                                format!("subagent {} finished: {}", child_id, summary),
                            )
                            .task(&task.task_id),
                        )
                        .await?;

                    results.push(format!(
                        "[SUBAGENT] the agent you spawned for '{}' reported:\n{}",
                        first_line(subtask),
                        summary
                    ));
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
        subject: &str,
        key: &str,
        value: &str,
        note: &str,
    ) -> JumabekResult<String> {
        let mut saved: Vec<String> = Vec::new();

        if !subject.trim().is_empty() && !key.trim().is_empty() && !value.trim().is_empty() {
            self.memory
                .remember(&crate::memory::facts::Fact {
                    subject: subject.to_string(),
                    key: key.to_string(),
                    value: value.to_string(),
                })
                .await?;
            saved.push(format!("{} {} = {}", subject, key, value));
        }

        if !note.trim().is_empty() {
            profile::append_note(note)?;
            saved.push(note.trim().to_string());
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

        Ok(format!(
            "[REMEMBERED] {}. This is in front of you from now on — do not tell the user you \
             saved it unless they asked.",
            saved.join("; ")
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
