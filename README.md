<img src="docs/banner.svg" alt="JumaBek" width="100%">

<p>
  <a href="../../actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/jumabek-dev/jumabek/ci.yml?branch=main&style=flat-square&label=ci&labelColor=1f2a37&color=a3d977"></a>
  <a href="../../releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/jumabek-dev/jumabek?style=flat-square&label=release&labelColor=1f2a37&color=5ccfe6"></a>
  <a href="../../releases/latest"><img alt="Downloads" src="https://img.shields.io/github/downloads/jumabek-dev/jumabek/total?style=flat-square&label=downloads&labelColor=1f2a37&color=f07178"></a>
  <img alt="Platforms" src="https://img.shields.io/badge/platform-windows%20%C2%B7%20linux%20%C2%B7%20macos-c3a6ff?style=flat-square&labelColor=1f2a37">
  <img alt="Rust" src="https://img.shields.io/badge/rust-2024%20edition-ffcc66?style=flat-square&labelColor=1f2a37">
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/core-AGPL--3.0-8aa0b8?style=flat-square&labelColor=1f2a37"></a>
  <a href="jumabek_sdk/LICENSE-MIT"><img alt="SDK license" src="https://img.shields.io/badge/sdk-MIT%20%7C%20Apache--2.0-8aa0b8?style=flat-square&labelColor=1f2a37"></a>
</p>

**An assistant that writes its own skills when it runs out of them.**

It runs on your machine, does real work on it, and when a task needs something it cannot do,
it says so, asks, and compiles the missing piece — each one checked inside a container before
it is allowed near your files.

[Install](#install) · [How it works](#how-it-works) · [Writing skills](#writing-skills) · [Safety](#safety) · [Limits](#what-it-does-not-do) · [Site](https://jumabek-dev.github.io/jumabek/)

---

## A real session

Asked for something it has no skill for, JumaBek notices, explains what is missing, and asks
before writing anything.

```console
> I need to parse RSS feeds regularly and pull out the headlines

  I have shell_executor, but it only runs commands. Parsing RSS needs an HTTP
  client and an XML parser, which a shell does not have. I suggest building a
  skill called rss_parser. Shall I start?

  permission  MEDIUM   write a new skill 'rss_parser' in Rust
  Write, build and install a new skill 'rss_parser' in Rust. The code is written
  by the model and built on this machine; once installed it loads in every
  future session.

  allow? [y/N] y
  allowed

  · rss_parser: preflight passed in docker engine 28.5.1 —
      build: 2 cpu / 2g ram, network on · run: 0.5 cpu / 256m, no network, read-only
  · rss_parser: built and validated
  · rss_parser is live: fetch_titles, fetch_titles_formatted

  · rss_parser · fetch_titles_formatted

  Here are the headlines from Hacker News: ...
```

Forty-nine seconds from the request to a working result. The skill stays installed and loads
on every later run.

---

## Install

<table>
<tr><td><b>Windows</b></td><td>

```powershell
irm https://raw.githubusercontent.com/jumabek-dev/jumabek/main/install.ps1 | iex
```

</td></tr>
<tr><td><b>Linux, macOS</b></td><td>

```bash
curl -fsSL https://raw.githubusercontent.com/jumabek-dev/jumabek/main/install.sh | bash
```

</td></tr>
</table>

The installer puts everything under `~/.jumabek`, adds it to your PATH, and never overwrites
a config you have already edited. It names the kinds of endpoint that work and installs none
of them for you.

From source, if you would rather:

```bash
cargo install --git https://github.com/jumabek-dev/jumabek
```

Then set a key and start:

```bash
export JUMABEK_API_KEY="your-key"     # or put it in ~/.jumabek/secrets.toml
jumabek
```

### Upgrading

Re-run the installer. It replaces the binaries and leaves `config.toml` and `secrets.toml`
alone, because those are yours once they exist.

`prompt.md` is the exception, and it has to be. It is how the model learns what it is
allowed to do, so a release that adds an action adds it there too — and a copy that never
moves ships a capability nobody can reach. Nothing fails; the feature is simply invisible.

So the binary carries the prompt it was built with and keeps the last reconciled one beside
yours as `prompt.md.release`. On startup:

- your copy is untouched → it is brought forward, and one line says so;
- your copy has your edits and the release has not moved → nothing is said;
- your copy has your edits and the release **has** moved → you are told, and nothing is
  overwritten. `jumabek doctor` names both files to compare.

Merging your own words into a new prompt is not something to guess at, so it is never
guessed at.

### What it needs

| Dependency | Without it |
| :--- | :--- |
| An OpenAI-compatible endpoint | nothing works |
| Rust toolchain | it runs, but cannot write itself Rust skills |
| Python 3 / Node | the same, for skills in those languages |
| Docker | new skills are refused, because they cannot be checked first |
| ffmpeg | voice is unavailable; typing still works |

`jumabek doctor` reports all of it, and says what each gap costs you:

```console
  ok   home         ~/.jumabek
  ok   config       ~/.jumabek/config.toml
  ok   API key      found
  ok   LLM          http://localhost:11434/v1 · qwen3.5:4b
  ok   intelligence one model for everything
  ok   Rust         cargo 1.96.0 — skills can be written in Rust
  ok   Python       Python 3.12.4 — skills can be written in Python
  WARN Node         node not found
       JumaBek runs, and can still write skills in the other languages
  WARN Docker       docker is installed but the engine is not running
       new skills are checked in a container before they touch your machine;
       without it building them is refused
  ok   skills       2 installed: shell_executor, rss_parser

  7 ok, 2 warning(s), 0 failure(s)
  JumaBek will run; the warnings above disable parts of it
```

> [!NOTE]
> **Any OpenAI-compatible endpoint.** Point `[llm].base_uri` at a local runner (Ollama, LM
> Studio, llama.cpp), at a router in front of several providers, or at a provider directly.
> The client sends `model`, `messages` and `stream`, and reads `choices[0].message.content`
> — nothing beyond that is assumed.
>
> Write the address the way its own documentation gives it: with `/v1` on the end or
> without, both land in the same place. An endpoint that wants no API key needs none —
> leave it unset and no `Authorization` header is sent.
>
> What a local model does **not** get you for free is the agent itself: every turn has to
> come back as one JSON object in a fixed action format, and a small model will miss it
> often. Local is a real option for routine work; treat "it connects" and "it can drive the
> loop" as separate questions.

---

## How it works

Every skill is a separate process. JumaBek writes a line of JSON to its stdin and reads a
line back from its stdout.

```jsonc
// core  ->
{"id":1,"method":"execute","params":"{\"method\":\"execute_command\",\"args\":\"ls\"}"}
// skill ->
{"id":1,"payload":{"Output":{"Text":"file1.txt\nfile2.txt"}}}
```

That is the whole contract, and it buys several things at once.

| | |
| :--- | :--- |
| **Any language** | A skill is whatever speaks the protocol — including the ones JumaBek writes itself, in Rust, Python or Node. |
| **Nothing to rebuild** | Adding a skill means dropping a binary in a folder. The agent itself is never recompiled. |
| **Crashes stay local** | A skill that hangs is killed and restarted on the next call. It cannot take the agent down. |
| **Lazy by default** | Descriptions are cached, so twenty installed skills cost one millisecond at startup instead of seven hundred. |

### Three models, and the sense to pick one

Turning a light on and writing a skill are not the same kind of thinking, and paying for them
at the same rate is not a decision anybody makes deliberately. Name three models and JumaBek
moves between them.

```toml
[llm.intelligence]
low     = "cc/claude-haiku-4-5-20251001"
medium  = "cc/claude-sonnet-4-6"
high    = "cc/claude-opus-4-8"
default = "medium"
```

The section is optional. Name none and nothing changes — one model, as before. Name only some
and switching stays off, with `jumabek doctor` saying which level has no model behind it; a
level that cannot be reached is worse than no levels at all.

`low` is one skill call and done. `medium` is the default: several steps, a search, files,
skills chained together. `high` is for writing a skill, or for anything that already failed a
level down.

The model can move itself, freely downwards and upwards with a stated reason. What it cannot
do is decide the cases that matter, because a model too weak for a task is in no position to
notice. So the core moves it, on events rather than opinions:

| | |
| :--- | :--- |
| **Writing a skill** | Always the highest level, before the first line of code. A cheap model has no way of telling that the skill it just wrote is bad, and the cost of being wrong is a binary that lives on your machine for months |
| **Two unreadable answers** | The model is not holding the response format. Another attempt at the same level is a wasted turn |
| **A build that keeps failing** | The code is above this level |
| **A task running out of iterations** | Circling, rather than working |
| **Nobody at the keyboard** | Scheduled jobs and anything arriving through the inbox start at `low` |

An escalation that was not the task's fault does not spend an iteration. The turn failed
because the level was wrong, and charging the task for that would quietly make the cheaper
level worse than never switching at all. Every task starts again at the default, so one hard
afternoon does not leave the expensive model running all week.

Each answered turn records the level it ran on, and the turn where the level moved records
why — otherwise there is no way to tell whether any of this earns its keep.

### Memory

Everything said is kept in SQLite. The current session is always in context; older sessions
are searched only when the model asks, through a full-text index with Russian and English
stemming — so `файл` finds `файлами`, and `file` finds `files`.

When a conversation outgrows the context window, the oldest exchanges are dropped in whole
task groups — never half of one, which would leave a result with no matching command — and
replaced by a marker telling the model what it can still recall.

### Sub-agents

Some work is worth doing but not worth reading. Scanning forty log files fills a context
window with output whose only useful part is the conclusion.

So JumaBek can hand a piece of work to a copy of itself. The copy starts empty — the system
prompt, the skills, and a task written as a standalone instruction. It cannot see the
conversation it came from, which is the entire point. It runs its own loop and returns one
summary.

```
  · subagent · read every .log under C:/logs and list the error codes
  · shell_executor · execute_command
  · subagent · done in 12.4s
```

Nesting stops at two levels. Below that, a tree is almost always a task that failed to
decompose and started looping on itself.

### Background jobs

A job is work that outlives the prompt: a reminder, a recurring check, a folder being
watched. Jobs live in SQLite and come back after a restart — most of what makes a reminder
worth setting.

| Schedule | Meaning |
| :--- | :--- |
| `in 3h` | once, three hours from now |
| `at 2026-07-30T09:00:00Z` | once, at a moment |
| `every 30m` | repeating, minimum 10s |
| `cron 0 9 * * 1-5` | five fields: minute hour day month weekday |
| `watch ~/Downloads` | when something there appears, changes or disappears |

Watching polls and compares name, size and modification time. Filesystem events would be
sharper, but they cost a dependency and a debouncing problem to save a few seconds on a job
that runs every quarter hour. The first look only learns what is there — otherwise every
watch would fire once at startup on a directory nobody touched.

Jobs report into the live session through rustyline's external printer, which redraws the
line you are typing underneath the message instead of through it.

### The inbox

Skills answer when called. That is fine until a skill is holding something live — a chat
connection, a folder, a webhook — and needs to speak first.

So the core listens on `127.0.0.1`. Anything on the machine with a token can push work in:

```console
$ curl -H "Authorization: Bearer $TOKEN" -d '{"source":"telegram","kind":"notify",
        "text":"Асия: буду через час","who":"asiya"}' localhost:20129/notify
{"status":"queued from telegram"}
```

`/notify` queues something that happened and answers immediately. `/ask` runs the request and
returns the reply over the same connection — which is how a bot JumaBek writes for itself
gets its answers back.

Three locks, and each is load-bearing:

**The address is not configurable.** Loopback, in a constant, with a test that fails if it
changes. A port that runs tasks on your machine is a shell; one reachable from the network is
somebody else's shell.

**Every caller has its own token.** Tokens live in `secrets.toml`, one per caller, so a
compromised one is revoked without touching the rest. Under 24 characters is ignored and said
so out loud, because a token quietly dropped is worse than one refused.

**A grant is required, not optional.** Rights live in `config.toml` and belong to the token,
never to the request — a caller cannot widen its own permissions by asking. Inbound work runs
under the same rules as a background job: it cannot ask you anything, and it cannot step
outside its list.

The model can ask for a key for a skill it wrote, and the core generates it, writes it and
hands it over — the model never sees the token and cannot edit those files itself.

### Changing settings while it runs

`config.toml`, `secrets.toml` and `prompt.md` are watched. Save one and it is picked up
within a few seconds:

```console
  · reloaded config.toml
  ·   max_iterations 10 -> 14
  ·   inbox now admits telegram, relay
```

Iteration limits, the model and endpoint, the API key, the prompt, inbox tokens and grants,
and per-skill settings all take effect live — a skill whose settings changed is restarted,
since its environment is handed to it when its process starts and nothing else reaches it.

Two things need a restart, and say so rather than silently doing nothing: the database path,
because the session is open inside it, and the inbox port, because the listener is already
bound.

---

## Writing skills

You can write one yourself, or let JumaBek do it. Either way it is one file.

```rust
use jumabek_sdk::{MethodInfo, ModuleMetadata, SkillError, SkillModule, SkillOutput};

struct WordCount { metadata: ModuleMetadata }

#[async_trait::async_trait]
impl SkillModule for WordCount {
    fn get_metadata(&self) -> &ModuleMetadata { &self.metadata }
    fn health_check(&self) -> bool { true }

    fn available_methods(&self) -> Vec<MethodInfo> {
        vec![MethodInfo {
            method: "count".to_string(),
            description: "Count the words in a piece of text".to_string(),
            args_description: "The text to count".to_string(),
        }]
    }

    async fn execute(&self, method: &str, args: &str) -> Result<SkillOutput, SkillError> {
        match method {
            "count" => Ok(SkillOutput::Text(args.split_whitespace().count().to_string())),
            other => Err(SkillError::NotFound(format!("unknown method '{}'", other))),
        }
    }
}

#[tokio::main]
async fn main() {
    jumabek_sdk::runtime::run_skill(WordCount { /* ... */ }).await.unwrap();
}
```

Build it, drop the binary in `~/.jumabek/skills`, and it is there next start.

### In another language

A skill is a process, so the language was never the protocol's business — only the build
pipeline's. JumaBek writes skills in **Rust, Python or Node**, and a skill it writes for
itself says which:

```json
{"type":"GenerateChunk","module_name":"weather","language":"python", ...}
```

Rust links `jumabek_sdk`. Python and Node get a small `jumabek` helper written next to the
code, so the wire format is never reimplemented by hand:

```python
import jumabek

def execute(method, args):
    if method == "count":
        return str(len(args.split()))
    raise jumabek.SkillError("unknown method: " + method, kind="NotFound")

jumabek.run(name="word_count", version="0.1.0",
            description="Counts the words in a piece of text",
            methods=[{"method": "count",
                      "description": "Count the words in a piece of text",
                      "args_description": "The text to count"}],
            execute=execute)
```

The helper points `print` and `console.log` at stderr, so a debug line left in by accident
cannot corrupt the response the core is parsing.

Rust installs as one binary. The others install as `~/.jumabek/skills/<name>.d/` — code,
helper and dependencies together — beside a launcher named `<name>`. The skill layer only
ever sees an executable path, which is why nothing else in the codebase has a special case
for them.

Each language builds in its own container image and its own package cache. Override them
per language when the defaults do not suit:

```toml
[preflight]
image = "rust:1-slim"        # kept under its old name: this is the Rust one

[preflight.images]
python = "python:3.12-slim"
node = "node:22-slim"
```

A language the machine does not have is refused with `[TOOLCHAIN MISSING]` before any code
is written to disk, and it costs the model none of its fix attempts — rewriting the code
would not have helped. `jumabek doctor` lists which of the three are usable here.

### Settings and keys

A skill runs with a stripped environment. It cannot see the agent's own credentials, and it
must never contain a hard-coded key. Whatever you put under `[skills.<name>]` reaches that
skill, and only that skill:

```toml
# config.toml                    # secrets.toml
[skills.weather]                 [skills.weather]
city = "Almaty"                  api_key = "..."
```

```
JUMABEK_SKILL_CITY=Almaty        JUMABEK_SKILL_API_KEY=...
```

---

## Commands

```bash
jumabek                          # start a session
jumabek "how many files here?"   # run one task and exit
jumabek --voice                  # speak instead of typing

jumabek doctor                   # check the setup
jumabek mic                      # watch the microphone level for ten seconds
jumabek where                    # print every path it uses

jumabek skills                   # list installed skills
jumabek remove <name>            # remove one

jumabek jobs                     # list background jobs
jumabek job-stop <id>            # stop and delete one

jumabek inbox                    # is the door open, and who may knock
jumabek profile                  # what it remembers about you
jumabek forget-subject <who>     # make it forget one subject

jumabek backups                  # list snapshots
jumabek restore <id>             # roll back to one
```

Inside a session, `/voice` and `/cli` switch modes without losing the conversation, and
`/quit` leaves. Shift+Enter starts a new line without submitting — Alt+Enter does the same
on terminals that do not report modifier keys.

Answers are rendered, not printed: headings, lists, tables, code blocks and emphasis all
arrive as terminal formatting rather than raw asterisks. Your turn and the agent's are
told apart by a chip against a solid left bar, so a long session stays readable.

### When voice does not hear you

A microphone that goes unheard used to be a silent failure with nothing to look at.
`jumabek mic` opens the device and shows the level against the threshold it has to beat:

```console
       0 |                              | needs     50   quiet
      39 |                              | needs     50   VOICE
     141 |#                             | needs     50   VOICE
      93 |#                             | needs     50   VOICE

  loudest frame: 146
  noise floor settled at: 19
  complete utterances: 1

  The microphone works and speech is being detected.

  The signal is quiet: 146 at its loudest, where speech usually reaches
  a few thousand. It clears the threshold, but transcription will be better
  with the input level raised in the system sound settings.
```

The threshold falls over the first second as the noise floor settles, so a quiet room ends
up more sensitive than a loud one. A sentence has to clear the line for half a second to
count, and finishes after nine hundred milliseconds of silence — which is why the check
waits for you to stop talking rather than cutting at the clock.

Voice mode says the same things as it goes: when it starts listening, how long an utterance
was, and when it heard something it could not make out.

---

## Safety

Self-improvement means running code that did not exist a minute ago. Five things stand
between that and your machine, and each exists because of something that actually went wrong.

**Dangerous commands are stopped by the core, not by the model.** Recursive deletes, disk
formatting, shutdown, a download piped into a shell — all need your word, whether or not the
model thought to ask. Relying on the model to volunteer is not a control: told to skip the
confirmation, it skips it.

**New code is exercised in a container first.** Built, then run with no network, a
read-only filesystem, capped CPU and memory, and every capability dropped. Code that hangs,
crashes or reaches for the network is caught there rather than on your disk.

In that container the skill is also **called by its own method names**. It used to be enough
to start, say your name and survive nonsense — so a skill whose only method answered "no
such method" when asked for itself passed every check and was installed, and the model found
out by calling it. Now each declared method is tried, and one that answers exactly the way
the skill answers a name it has never heard of fails the build. This happens only inside the
container, where there is no network and nothing to write to: calling a stranger's methods
to see what they do is safe there and nowhere else.

**Every install is preceded by a snapshot.** Rolling back removes a skill that did not exist
at that point, rather than merely restoring the files that did. The rollback itself is
snapshotted first.

**Skills cannot leak processes.** Each runs inside a group killed as a unit, so a shell
command it started does not outlive it — even if the agent itself is killed.

**Nothing gets in without a grant.** A background job's rights are fixed before it runs, and
so are an inbound request's. Everything else here asks at the
moment it matters. A job cannot: there is nobody at the prompt at three in the morning. So
approving one means approving a list of skills, and separately whether it may write new
skills or step past a safety rule — and the question leads with that list rather than with
the task. A job that tries anything else is refused and says so in its report; it cannot
ask, and it cannot delegate its way around the limit, because a sub-agent inherits the
grant that spawned it.

---

## What it does not do

> [!WARNING]
> **The container is a check, not a jail.** It catches broken and misbehaving code before
> installation. It does not protect against a malicious build script in a dependency,
> because the binary that finally gets installed is compiled natively afterwards. That is
> why the config section is called `preflight` and not `sandbox`.

**A local model connects; that is not the same as working.** The transport has been checked
against Ollama, Ollama Cloud and OmniRoute, with a key and without. What a small local model
does not give you is the agent itself: every turn has to come back as one JSON object in a
fixed action format, and a 4B model will miss it often. Treat "it connects" and "it can drive
the loop" as separate questions.

**Voice is only half proven.** Capture has now met real hardware: the device is found, the
stream arrives, and speech is detected against the noise floor on a USB headset — that much
is measured, not assumed. What has not been exercised end to end is the rest of the round
trip, transcription through to a spoken answer. The race that made older assistants listen
to their own voice is fixed and measured. The detection thresholds are still tuned to one
room and one microphone; `jumabek mic` will tell you how yours compares.

**Parallelism helps across skills, not within one.** Two calls to the same skill share one
connection and one working directory, so they are deliberately serialised.

---

## License

Three licenses, because the parts are not the same kind of thing.

| Part | License | |
| :--- | :--- | :--- |
| The agent (`jumabek`) | [AGPL-3.0](LICENSE) | |
| The skill SDK (`jumabek_sdk`) | [MIT](jumabek_sdk/LICENSE-MIT) or [Apache-2.0](jumabek_sdk/LICENSE-APACHE) | |
| The shipped skills (`skills/*`) | [MIT](skills/LICENSE) | |

**The agent is AGPL** because the one thing worth guarding against is somebody running it as
a hosted service and keeping their changes. AGPL does not forbid that; it just requires the
changes to come back. Running JumaBek on your own machine, modifying it, or writing skills
for it are unaffected — that is the whole point of it.

**The SDK is permissive** because a skill links it. Under AGPL every third-party skill would
inherit the same terms, which would end the idea of an ecosystem before it started. The
license boundary sits exactly where the process boundary already sits: skills are separate
programs speaking a protocol, and they are yours.

Contributions require a [CLA](CONTRIBUTING.md), so the licensing can still be adjusted later
without hunting down everyone who ever sent a patch.

Juma — Friday in Kazakh; the one that came after Jarvis.
