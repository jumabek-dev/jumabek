use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use jumabek_sdk::{MethodInfo, ModuleMetadata, SkillError, SkillModule, SkillOutput};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT_CHARS: usize = 200_000;
#[cfg(target_os = "windows")]
const PS_OUTPUT_WIDTH: u32 = 4096;

const NO_CALLER: &str = "__default__";

fn caller_key() -> String {
    jumabek_sdk::caller().unwrap_or_else(|| NO_CALLER.to_string())
}

fn process_default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[derive(Debug)]
pub struct ShellExecutor {
    pub metadata: ModuleMetadata,
    virtual_cwd: Mutex<HashMap<String, PathBuf>>,
}

impl Default for ShellExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellExecutor {
    pub fn new() -> Self {
        ShellExecutor {
            metadata: ModuleMetadata {
                name: "shell_executor".to_string(),
                version: "1.0.0".to_string(),
                description: "Execute any shell command or program (cross-platform: \
                    bash/sh on Unix, PowerShell on Windows; UTF-8 safe, timeout + \
                    process-tree kill, virtual cwd)"
                    .to_string(),
            },
            virtual_cwd: Mutex::new(HashMap::new()),
        }
    }

    fn cwd_of(&self, caller: &str) -> PathBuf {
        self.virtual_cwd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(caller)
            .cloned()
            .unwrap_or_else(process_default_cwd)
    }

    fn try_intercept_cd(&self, cmd: &str) -> Option<Result<String, String>> {
        let trimmed = cmd.trim();

        if trimmed.contains([';', '|', '&', '>', '<', '`', '\n', '\r']) {
            return None;
        }

        let lower = trimmed.to_lowercase();
        let raw_arg: &str =
            if lower == "cd" || lower == "chdir" || lower == "set-location" || lower == "sl" {
                ""
            } else if lower.starts_with("cd ") {
                trimmed[3..].trim()
            } else if lower.starts_with("chdir ") {
                trimmed["chdir ".len()..].trim()
            } else if lower.starts_with("set-location ") {
                trimmed["set-location ".len()..].trim()
            } else if lower.starts_with("sl ") {
                trimmed[3..].trim()
            } else {
                return None;
            };

        let unquoted = strip_quotes(raw_arg);
        let expanded = expand_vars(unquoted);

        let caller = caller_key();
        let here = self.cwd_of(&caller);

        let target: PathBuf = if expanded.is_empty() {
            match home_dir() {
                Some(h) => h,
                None => here.clone(),
            }
        } else {
            let p = Path::new(&expanded);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                here.join(p)
            }
        };

        match target.canonicalize() {
            Ok(resolved) if resolved.is_dir() => {
                let display = strip_unc(&resolved);
                self.virtual_cwd
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(caller, resolved);
                Some(Ok(format!("Changed directory to: {}", display)))
            }
            Ok(resolved) => Some(Err(format!("Not a directory: {}", resolved.display()))),
            Err(e) => Some(Err(format!("cd: cannot access '{}': {}", expanded, e))),
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn pick_unix_shell() -> &'static str {
    const CANDIDATES: [&str; 4] = [
        "/bin/bash",
        "/usr/bin/bash",
        "/usr/local/bin/bash",
        "/opt/homebrew/bin/bash",
    ];
    for p in CANDIDATES {
        if Path::new(p).exists() {
            return p;
        }
    }
    "/bin/sh"
}

fn decode_console_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }

    #[cfg(target_os = "windows")]
    {
        let (oem, ..) = encoding_rs::IBM866.decode(bytes);
        if !oem.contains('\u{FFFD}') {
            return oem.into_owned();
        }
        let (ansi, ..) = encoding_rs::WINDOWS_1251.decode(bytes);
        if !ansi.contains('\u{FFFD}') {
            return ansi.into_owned();
        }
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[cfg(not(target_os = "windows"))]
    {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn kills_by_pattern(command: &str) -> bool {
    let low = command.to_lowercase();
    low.contains("pkill -f") || low.contains("pkill -9 -f") || low.contains("killall")
}

fn truncate_text(s: String) -> String {
    match s.char_indices().nth(MAX_OUTPUT_CHARS) {
        Some((idx, _)) => format!(
            "{}\n... [output truncated at {} chars]",
            &s[..idx],
            MAX_OUTPUT_CHARS
        ),
        None => s,
    }
}

fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn expand_vars(input: &str) -> String {
    let work = if input == "~" || input.starts_with("~/") || input.starts_with("~\\") {
        match home_dir() {
            Some(h) => format!("{}{}", h.display(), &input[1..]),
            None => input.to_string(),
        }
    } else {
        input.to_string()
    };

    let chars: Vec<char> = work.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(work.len());
    let mut i = 0;

    while i < n {
        let c = chars[i];

        if c == '%' {
            if let Some(j) = (i + 1..n).find(|&j| chars[j] == '%') {
                let name: String = chars[i + 1..j].iter().collect();
                if !name.is_empty() {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                    i = j + 1;
                    continue;
                }
            }
        } else if c == '$' {
            if i + 1 < n && chars[i + 1] == '{' {
                if let Some(j) = (i + 2..n).find(|&j| chars[j] == '}') {
                    let mut name: String = chars[i + 2..j].iter().collect();
                    if let Some(rest) = name.strip_prefix("env:") {
                        name = rest.to_string();
                    }
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                    i = j + 1;
                    continue;
                }
            } else {
                let mut k = i + 1;
                let tail: String = chars[k..].iter().collect();
                if tail.to_lowercase().starts_with("env:") {
                    k += 4;
                }
                let start = k;
                while k < n && (chars[k].is_alphanumeric() || chars[k] == '_') {
                    k += 1;
                }
                if k > start {
                    let name: String = chars[start..k].iter().collect();
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                    i = k;
                    continue;
                }
            }
        }

        out.push(c);
        i += 1;
    }

    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn strip_unc(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(s)
}

async fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }

    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output()
            .await;
    }
}

#[async_trait::async_trait]
impl SkillModule for ShellExecutor {
    fn get_metadata(&self) -> &ModuleMetadata {
        &self.metadata
    }

    fn health_check(&self) -> bool {
        true
    }

    fn available_methods(&self) -> Vec<MethodInfo> {
        vec![MethodInfo {
            method: String::from("execute_command"),
            description: String::from(
                "Execute ANY shell command or program and return its combined output \
                 (run programs, build projects, write files, run python/node scripts, \
                 git, package managers, etc.).",
            ),
            args_description: String::from(
                "A single command string.\n\
                 - Windows: run through PowerShell (UTF-8 forced, ExecutionPolicy \
                 Bypass). Use PowerShell syntax, e.g. Set-Content -Path file.py -Value '...', \
                 python script.py, Get-ChildItem C:\\Users, cargo build.\n\
                 - Linux/macOS: run through bash (falls back to sh). Full bash syntax \
                 works: pipes, &&, redirects, heredocs (cat > f.py << 'EOF' ... EOF), \
                 subshells. E.g. python3 script.py, ls -la /home.\n\
                 Pipes/redirects and multi-statement commands (a; b | c) work. Native \
                 exit codes are honored (a successful cargo build is NOT a failure). \
                 To write a file, either redirect/heredoc (bash) or use Set-Content/Out-File \
                 (PowerShell), then run it as a separate or chained command.\n\
                 A STANDALONE cd/Set-Location persists across calls (virtual cwd, since \
                 each call is a fresh process); inside a compound command it only affects \
                 that one invocation - prefer passing paths explicitly (--manifest-path, \
                 -Path). Commands are killed after 300s.\n\
                 STDIN IS CLOSED: nothing can be typed in response to a prompt, so any \
                 command that waits for input hangs until the timeout kills it. Always \
                 pass non-interactive flags (apt-get -y, npm init -y, git commit -m, \
                 ssh -o BatchMode=yes) or pipe the answers in.\n\
                 Output is truncated at 200000 characters, with a marker at the cut - \
                 filter noisy commands (Select-Object -First, head, grep) instead of \
                 dumping whole logs.\n\
                 On failure the error text carries the exit code plus the captured \
                 [STDOUT] and [STDERR] sections, so a non-zero exit still shows whatever \
                 the command printed before dying - read them before retrying.",
            ),
        }]
    }

    async fn execute(&self, method: &str, args: &str) -> Result<SkillOutput, SkillError> {
        if method != "execute_command" {
            return Err(SkillError::NotFound(format!(
                "Unknown method '{}', expected 'execute_command'",
                method
            )));
        }
        if args.trim().is_empty() {
            return Err(SkillError::InvalidArgs("Empty command".to_string()));
        }

        if let Some(result) = self.try_intercept_cd(args) {
            return match result {
                Ok(message) => Ok(SkillOutput::Text(message)),
                Err(message) => Err(SkillError::ExecutionFailed(message)),
            };
        }

        let cwd = self.cwd_of(&caller_key());

        let mut command;

        #[cfg(target_os = "windows")]
        {
            let wrapped = format!(
                "$ProgressPreference='SilentlyContinue'; \
                [Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                [Console]::InputEncoding=[System.Text.Encoding]::UTF8; \
                $OutputEncoding=[System.Text.Encoding]::UTF8; \
                $PSDefaultParameterValues['Get-Content:Encoding']='utf8'; \
                $ErrorActionPreference='Stop'; \
                try {{ \
                    & {{\n{}\n}} | Out-String -Width {}; \
                    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }} \
                    exit 0 \
                }} catch {{ \
                    [Console]::Error.WriteLine($_.Exception.Message); \
                    exit 1 \
                }}",
                args, PS_OUTPUT_WIDTH
            );

            command = tokio::process::Command::new("powershell");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &wrapped,
            ]);
        }

        #[cfg(not(target_os = "windows"))]
        {
            command = tokio::process::Command::new(pick_unix_shell());
            command.args(["-c", args]);
        }

        command.current_dir(&cwd);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.stdin(std::process::Stdio::null());
        command.kill_on_drop(true);

        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let child = command.spawn().map_err(|e| {
            SkillError::ExecutionFailed(format!("Failed to execute command: {}", e))
        })?;
        let pid = child.id();

        let output = match tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(SkillError::ExecutionFailed(format!(
                    "Failed to wait for command: {}",
                    e
                )));
            }
            Err(_elapsed) => {
                kill_process_tree(pid).await;
                return Err(SkillError::ExecutionFailed(format!(
                    "[TIMEOUT] Command exceeded {}s and was killed: {}",
                    COMMAND_TIMEOUT.as_secs(),
                    args
                )));
            }
        };

        let stdout = truncate_text(decode_console_output(&output.stdout));
        let stderr = truncate_text(decode_console_output(&output.stderr));

        if !output.status.success() {
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());

            let mut msg = format!("Command exited with status {}", code);

            let low = stderr.to_lowercase();
            if low.contains("access denied")
                || low.contains("permission denied")
                || low.contains("requires elevation")
                || low.contains("отказано")
                || low.contains("доступ")
            {
                msg.push_str(
                    " (looks like a permissions problem — on Windows run elevated, \
                     on Unix consider sudo)",
                );
            } else if low.contains("command not found")
                || low.contains("is not recognized")
                || low.contains("no such file or directory")
            {
                msg.push_str(
                    " (program not found — check it is installed and on PATH; \
                     JumaBek inherits the PATH of the process that launched it)",
                );
            } else if output.status.code().is_none() && kills_by_pattern(args) {
                msg.push_str(
                    " (killed by a signal, and the command matches processes by pattern — \
                     pkill -f and killall match whole command lines, including the line of \
                     the shell running this very command, so the pattern killed the shell \
                     itself and nothing after it ran. Match something longer that only the \
                     target has, such as pkill -f '/node_modules/.bin/vite', or find the pid \
                     first and kill that pid)",
                );
            }

            if !stdout.trim().is_empty() {
                msg.push_str(&format!("\n[STDOUT]\n{}", stdout.trim()));
            }
            if !stderr.trim().is_empty() {
                msg.push_str(&format!("\n[STDERR]\n{}", stderr.trim()));
            }
            return Err(SkillError::ExecutionFailed(msg));
        }

        let mut result_text = if stdout.trim().is_empty() {
            "Command executed successfully (no output)".to_string()
        } else {
            stdout
        };
        if !stderr.trim().is_empty() {
            result_text.push_str(&format!("\n[STDERR]\n{}", stderr.trim()));
        }

        Ok(SkillOutput::Text(result_text))
    }
}

#[tokio::main]
async fn main() {
    jumabek_sdk::runtime::run_skill(ShellExecutor::new())
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_caller_starts_from_the_process_directory() {
        let skill = ShellExecutor::new();
        assert_eq!(skill.cwd_of("agent-a"), process_default_cwd());
        assert_eq!(skill.cwd_of("agent-b"), process_default_cwd());
    }

    #[test]
    fn one_callers_cd_does_not_move_another() {
        let skill = ShellExecutor::new();
        let elsewhere = std::env::temp_dir().canonicalize().expect("temp dir");

        skill
            .virtual_cwd
            .lock()
            .unwrap()
            .insert("agent-a".to_string(), elsewhere.clone());

        assert_eq!(skill.cwd_of("agent-a"), elsewhere);
        assert_eq!(
            skill.cwd_of("agent-b"),
            process_default_cwd(),
            "one agent's cd relocated another"
        );
    }

    #[test]
    fn a_caller_that_sends_no_identity_gets_one_shared_directory() {
        assert_eq!(caller_key(), NO_CALLER, "outside a call there is no caller");

        let skill = ShellExecutor::new();
        let elsewhere = std::env::temp_dir().canonicalize().expect("temp dir");
        skill
            .virtual_cwd
            .lock()
            .unwrap()
            .insert(NO_CALLER.to_string(), elsewhere.clone());

        assert_eq!(skill.cwd_of(NO_CALLER), elsewhere);
    }

    #[test]
    fn a_pattern_kill_is_recognised_however_it_is_written() {
        for command in [
            "pkill -f vite",
            "pkill -9 -f vite",
            "killall node",
            "echo start; PKILL -F Vite; echo done",
        ] {
            assert!(kills_by_pattern(command), "missed: {command}");
        }
    }

    #[test]
    fn an_ordinary_kill_is_left_alone() {
        for command in ["kill -9 4213", "ls -la", "cargo build", "kill $(cat pid)"] {
            assert!(
                !kills_by_pattern(command),
                "would blame self-termination for: {command}"
            );
        }
    }
}
