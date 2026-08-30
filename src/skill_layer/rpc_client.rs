use std::collections::BTreeMap;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use jumabek_sdk::protocol::{ExecuteParams, SkillRequest, SkillResponse, SkillResponsePayload};
use jumabek_sdk::{MethodInfo, ModuleMetadata, SkillError, SkillModule, SkillOutput};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::error::{JumabekError, JumabekResult};
use crate::skill_layer::environment;
use crate::skill_layer::process_group::ProcessGroup;

struct Pipes {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    group: ProcessGroup,
}

pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(360);

pub struct SkillRpcClient {
    metadata: ModuleMetadata,
    methods: Vec<MethodInfo>,
    alive: AtomicBool,
    pipes: Mutex<Pipes>,
    binary: Option<PathBuf>,
    timeout: Duration,
    settings: BTreeMap<String, String>,
}

impl SkillRpcClient {
    #[cfg(test)]
    pub async fn spawn(path: &Path) -> JumabekResult<Self> {
        Self::spawn_with_settings(path, BTreeMap::new()).await
    }

    pub async fn spawn_with_settings(
        path: &Path,
        settings: BTreeMap<String, String>,
    ) -> JumabekResult<Self> {
        let mut command = Command::new(path);
        environment::apply(&mut command, &settings);

        let mut client = Self::spawn_command(command, &path.display().to_string()).await?;
        client.binary = Some(path.to_path_buf());
        client.settings = settings;
        Ok(client)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn spawn_command(mut command: Command, label: &str) -> JumabekResult<Self> {
        let path = std::path::PathBuf::from(label);
        let mut group = ProcessGroup::new();
        group.prepare(&mut command);

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                JumabekError::InternalError(format!(
                    "failed to spawn skill '{}': {}",
                    path.display(),
                    e
                ))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            JumabekError::InternalError(format!("skill '{}' has no stdin pipe", path.display()))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            JumabekError::InternalError(format!("skill '{}' has no stdout pipe", path.display()))
        })?;

        if let Some(pid) = child.id() {
            group.adopt(pid);
        }

        let mut pipes = Pipes {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            group,
        };

        let metadata = match Self::exchange(&mut pipes, "get_metadata", None)
            .await?
            .payload
        {
            SkillResponsePayload::Metadata(m) => m,
            other => {
                return Err(JumabekError::InternalError(format!(
                    "skill '{}' answered get_metadata with {:?}",
                    path.display(),
                    other
                )));
            }
        };

        let methods = match Self::exchange(&mut pipes, "available_methods", None)
            .await?
            .payload
        {
            SkillResponsePayload::Methods(m) => m,
            other => {
                return Err(JumabekError::InternalError(format!(
                    "skill '{}' answered available_methods with {:?}",
                    path.display(),
                    other
                )));
            }
        };

        Ok(SkillRpcClient {
            metadata,
            methods,
            alive: AtomicBool::new(true),
            pipes: Mutex::new(pipes),
            binary: None,
            timeout: DEFAULT_CALL_TIMEOUT,
            settings: BTreeMap::new(),
        })
    }

    async fn exchange(
        pipes: &mut Pipes,
        method: &str,
        params: Option<String>,
    ) -> JumabekResult<SkillResponse> {
        let request = SkillRequest {
            id: pipes.next_id,
            method: method.to_string(),
            params,
        };
        pipes.next_id += 1;

        let json = serde_json::to_string(&request)
            .map_err(|e| JumabekError::ParseError(format!("cannot encode request: {}", e)))?;

        pipes.stdin.write_all(json.as_bytes()).await?;
        pipes.stdin.write_all(b"\n").await?;
        pipes.stdin.flush().await?;

        loop {
            let mut line = String::new();
            let read = pipes.stdout.read_line(&mut line).await?;
            if read == 0 {
                return Err(JumabekError::InternalError(
                    "skill closed its stdout (process died)".to_string(),
                ));
            }

            let response = serde_json::from_str::<SkillResponse>(&line).map_err(|e| {
                JumabekError::ParseError(format!(
                    "malformed response from skill: {} — got: {}",
                    e, line
                ))
            })?;

            if response.id < request.id {
                continue;
            }

            if response.id > request.id {
                return Err(JumabekError::InternalError(format!(
                    "response id from the future: expected {}, got {}",
                    request.id, response.id
                )));
            }

            return Ok(response);
        }
    }

    pub async fn call(&self, method: &str, params: Option<String>) -> JumabekResult<SkillResponse> {
        let mut pipes = self.pipes.lock().await;

        if !self.alive.load(Ordering::Relaxed) {
            match self.restart(&mut pipes).await {
                Ok(()) => {
                    eprintln!("[skill_layer] restarted '{}'", self.metadata.name);
                }
                Err(e) => return Err(e),
            }
        }

        let result =
            match tokio::time::timeout(self.timeout, Self::exchange(&mut pipes, method, params))
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    pipes.group.kill_all();
                    let _ = pipes.child.kill().await;
                    Err(JumabekError::SkillError(SkillError::ExecutionFailed(
                        format!(
                            "'{}' did not answer within {} and was killed; \
                         it will be restarted on the next call",
                            self.metadata.name,
                            humanise(self.timeout)
                        ),
                    )))
                }
            };

        if result.is_err() {
            self.alive.store(false, Ordering::Relaxed);
        }

        result
    }

    async fn restart(&self, pipes: &mut Pipes) -> JumabekResult<()> {
        let Some(binary) = &self.binary else {
            return Err(JumabekError::InternalError(format!(
                "'{}' died and cannot be restarted",
                self.metadata.name
            )));
        };

        pipes.group.kill_all();
        let _ = pipes.child.kill().await;

        let mut group = ProcessGroup::new();
        let mut command = Command::new(binary);
        environment::apply(&mut command, &self.settings);
        group.prepare(&mut command);

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                JumabekError::InternalError(format!(
                    "cannot restart '{}': {}",
                    self.metadata.name, e
                ))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            JumabekError::InternalError("restarted skill has no stdin pipe".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            JumabekError::InternalError("restarted skill has no stdout pipe".to_string())
        })?;

        if let Some(pid) = child.id() {
            group.adopt(pid);
        }

        *pipes = Pipes {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            group,
        };

        self.alive.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_metadata_cached(&self) -> &ModuleMetadata {
        &self.metadata
    }

    pub fn methods_cached(&self) -> &[MethodInfo] {
        &self.methods
    }

    pub fn health_check_flag(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub async fn shutdown(&self) -> JumabekResult<()> {
        let mut pipes = self.pipes.lock().await;
        pipes.group.kill_all();
        pipes.child.kill().await?;
        self.alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait::async_trait]
impl SkillModule for SkillRpcClient {
    fn get_metadata(&self) -> &ModuleMetadata {
        &self.metadata
    }

    fn health_check(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn available_methods(&self) -> Vec<MethodInfo> {
        self.methods.clone()
    }

    async fn execute(&self, method: &str, args: &str) -> Result<SkillOutput, SkillError> {
        let params = ExecuteParams {
            method: method.to_string(),
            args: args.to_string(),
            caller: crate::skill_layer::current_caller(),
        };
        let encoded = serde_json::to_string(&params)
            .map_err(|e| SkillError::InvalidArgs(format!("cannot encode arguments: {}", e)))?;

        let response = self
            .call("execute", Some(encoded))
            .await
            .map_err(|e| SkillError::ExecutionFailed(e.to_string()))?;

        match response.payload {
            SkillResponsePayload::Output(output) => Ok(output),
            SkillResponsePayload::Error(err) => Err(err),
            other => Err(SkillError::ExecutionFailed(format!(
                "skill '{}' answered execute with {:?}",
                self.metadata.name, other
            ))),
        }
    }
}

fn humanise(span: Duration) -> String {
    if span < Duration::from_secs(1) {
        format!("{}ms", span.as_millis())
    } else {
        format!("{}s", span.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn probe_binary() -> PathBuf {
        let mut dir = std::env::current_exe().expect("test executable has a path");
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join(if cfg!(windows) {
            "shell_executor.exe"
        } else {
            "shell_executor"
        })
    }

    #[tokio::test]
    async fn a_silent_skill_is_killed_instead_of_hanging_forever() {
        let client = SkillRpcClient::spawn(&probe_binary())
            .await
            .expect("shell_executor must be built for this test")
            .with_timeout(Duration::from_secs(3));

        let params = serde_json::json!({
            "method": "execute_command",
            "args": if cfg!(windows) { "Start-Sleep -Seconds 60" } else { "sleep 60" }
        })
        .to_string();

        let started = std::time::Instant::now();
        let result = client.call("execute", Some(params)).await;

        assert!(result.is_err(), "a hanging call returned Ok");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "waited {:?} instead of giving up",
            started.elapsed()
        );
        assert!(!client.health_check_flag(), "skill still marked alive");
    }

    #[tokio::test]
    async fn a_dead_skill_is_restarted_on_the_next_call() {
        let client = SkillRpcClient::spawn(&probe_binary())
            .await
            .expect("shell_executor must be built for this test");

        client.shutdown().await.expect("the skill can be killed");
        assert!(!client.health_check_flag(), "shutdown left it marked alive");

        let quick = serde_json::json!({
            "method": "execute_command",
            "args": "echo back"
        })
        .to_string();

        let recovered = client.call("execute", Some(quick)).await;
        assert!(recovered.is_ok(), "skill never came back: {:?}", recovered);
        assert!(
            client.health_check_flag(),
            "still marked dead after restart"
        );
    }
}

#[cfg(test)]
mod orphan_tests {
    use super::*;
    use std::time::Duration;

    fn probe_binary() -> PathBuf {
        let mut dir = std::env::current_exe().expect("test executable has a path");
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join(if cfg!(windows) {
            "shell_executor.exe"
        } else {
            "shell_executor"
        })
    }

    fn command_lines() -> Vec<String> {
        let output = if cfg!(windows) {
            std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Get-CimInstance Win32_Process | ForEach-Object { $_.CommandLine }",
                ])
                .output()
        } else {
            std::process::Command::new("ps")
                .args(["-eo", "args="])
                .output()
        }
        .expect("the system must be able to list its own processes");

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.to_string())
            .collect()
    }

    fn marker_count(marker: &str) -> usize {
        command_lines()
            .iter()
            .filter(|line| line.contains(marker))
            .count()
    }

    #[tokio::test]
    async fn killing_a_skill_takes_its_children_with_it() {
        let marker = "jumabek_orphan_probe_marker";

        let client = SkillRpcClient::spawn(&probe_binary())
            .await
            .expect("shell_executor must be built")
            .with_timeout(Duration::from_secs(3));

        let command = if cfg!(windows) {
            format!("Start-Sleep -Seconds 45 # {}", marker)
        } else {
            format!("sleep 45 # {}", marker)
        };
        let params =
            serde_json::json!({ "method": "execute_command", "args": command }).to_string();

        let result = client.call("execute", Some(params)).await;
        assert!(result.is_err(), "the slow call should have timed out");

        tokio::time::sleep(Duration::from_millis(1500)).await;

        let survivors = marker_count(marker);
        assert_eq!(
            survivors, 0,
            "{} orphaned process(es) outlived the skill",
            survivors
        );
    }
}
