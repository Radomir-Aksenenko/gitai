# gitai

An issue goes in, a reviewed pull request comes out. Self-hosted, works against
Gitea, Forgejo, GitHub and GitLab, and runs whatever models you point it at,
including local ones.

Every stage is implemented and the loop runs end to end. What it has not had
yet is a real repository at scale, with real models, over weeks.

## How a task moves

```
issue labelled `gitai`
        |
        v
   planner            strong model. Writes the spec every later stage is judged against.
        |
        v
   N workers          cheap models, in parallel, each in its own sandbox
        |
        v
   THE GATE           build, tests, lint, scope, size. No model gets a vote here.
        |
        +--> failed --> editor writes precise instructions --> back to the worker
        |                                                       (inner loop)
        v
   reviewer           one per surviving attempt. Grades against the spec.
        |
        v
   arbiter            strong model. Picks the winner, or sends the round back.
        |                                                       (outer loop)
        v
   pull request  -->  a human
```

The gate is the part that makes the rest work. A hundred iterations of models
grading each other converge on code that reads well and does not run, so
nothing reaches a reviewer until it compiles and the tests pass. Model
judgement is applied to what a test suite cannot see: whether the change does
the right thing, and whether it is worth a maintainer's time.

Both loops are bounded by a budget in rounds, attempts, iterations, tokens,
wall clock and dollars. The two that matter most in practice are
`max_iterations` (how long one attempt is allowed to flail) and `max_cost_usd`.

## Quick Start with Docker (Recommended)

Run GitAI with zero local build prerequisites using Docker:

```bash
# 1. Copy the environment template and add your API keys
cp .env.example .env

# 2. Start GitAI container
docker compose up -d

# 3. Check health and configuration
docker compose exec gitai gitai doctor
```

For full documentation and all-in-one stack options, see **[docs/docker.md](docs/docker.md)**.

## Getting started (from source)

```bash
cargo build --release
```

```bash
./target/release/gitai init
```

That writes `gitai.toml` and the five prompt templates. Then:

1. Fill in `[gate]` with the commands that build and test your repository.
   Nothing else matters as much.
2. Point `[models]` and `[roles]` at models you can reach. Local models need
   only a `base_url`; `kind = "openai"` covers Ollama, vLLM, LM Studio,
   llama.cpp, OpenRouter and OpenAI itself.
3. Fill in `[forges.*]` with a token and a webhook secret.

```bash
./target/release/gitai doctor
```

`doctor` calls every configured model, checks the sandbox backend, validates
every cross-reference in the config, and refuses to be quiet about an empty
gate. It exits non-zero when something would fail at run time.

### Try it with no network at all

The `mock` provider answers canned JSON, so the whole pipeline runs offline:

```bash
gitai run --repo /path/to/a/checkout --title "Cache is never invalidated" --body "Stale reads after a write."
```

With `[roles]` pointed at a mock model this finishes in about a second and
leaves a `gitai/issue-0/r0-a0` branch behind. It proves the wiring, not the
models.

### Serving webhooks

Full deployment guides: **[docs/deploy-linux.md](docs/deploy-linux.md)** for a
Linux host with a self-hosted Gitea, and **[docs/deploy-github.md](docs/deploy-github.md)**
for public github.com. `deploy/` has a Gitea compose file and a systemd unit.

```bash
gitai serve
```

Each `[forges.<name>]` block becomes an endpoint at `/webhooks/<name>`.
Configure the hook with content type `application/json` and the same secret,
then label an issue. Deliveries with a bad signature are refused, and an empty
`webhook_secret` is a configuration error rather than a way to skip the check.

| Endpoint | What it does |
| --- | --- |
| `POST /webhooks/{forge}` | Issue events. Verified, then queued. |
| `GET /api/tasks` | Recent tasks, optionally `?state=working`. |
| `GET /api/tasks/{id}` | One task with all its attempts. |
| `GET /api/tasks/{id}/events` | Server-sent events, resumable with `?after=`. |
| `GET /healthz` | Liveness. |

## Where code runs

Model-written code executes behind the `Sandbox` trait and nowhere else.

The Docker backend gives each attempt its own container with `--network none`,
`--cap-drop ALL`, `no-new-privileges`, a pids limit, and CPU and memory caps.
The container is held open for the life of the attempt so that dependency
installation survives between gate steps. Dependency fetching belongs in
`gate.setup`, which runs before the network is cut.

The `local` backend runs commands straight on the host. It exists so the
pipeline can be developed without Docker installed. It is not an isolation
boundary and says so on every startup.

Git always runs on the host, never in the sandbox, so generated code never
holds a credentialed remote.

## Layout

| Crate | What lives there |
| --- | --- |
| `gitai-core` | Domain types and the four traits: `Forge`, `LlmProvider`, `Sandbox`, `Store`. No I/O. |
| `gitai-llm` | OpenAI-compatible, Anthropic and mock providers, plus the model router. |
| `gitai-forge` | Gitea/Forgejo, GitHub and GitLab. Webhook authentication, issues, pull requests. |
| `gitai-sandbox` | Checkout management, Docker and local backends, the gate. |
| `gitai-store` | SQLite: tasks, attempts, event log, job queue. |
| `gitai-pipeline` | The five roles and the two loops. Start with `engine.rs`. |
| `gitai-server` | axum: webhook intake, read API, worker pool. |
| `gitai-cli` | The `gitai` binary. |

Prompts live in `prompts/*.md` as minijinja templates. They are read at
startup, so tuning them needs no rebuild. The compiled-in copies are the
fallback when the directory is missing.

## Notes on the parts that are easy to get wrong

**Branches.** Every attempt that clears the gate is pushed, because the winner
is not known until after the workspaces are torn down. Once the task ends,
either way, the branches that were not selected are deleted through the forge
API. The diffs stay in the event log, so a post-mortem loses nothing. Set
`delete_rejected_branches = false` on a forge if you want the refs themselves.
A `gitai run` against a local checkout has no forge behind it, so its branches
are always left in place for you to look at.

**The queue.** SQLite, with `BEGIN IMMEDIATE` and a lease column instead of
`SKIP LOCKED`. Two things this got wrong at first and no longer does: sqlx's
`begin()` issues a plain deferred `BEGIN`, and upgrading that read lock to a
write lock makes SQLite return `SQLITE_BUSY` immediately no matter what
`busy_timeout` says. There are tests that claim the same job from two
independent connections to one file. Postgres would still be the right answer
for a hosted multi-node tier, and `Store` is the seam it goes through.

**Token budgets.** Context is planned in tokens, not bytes, using a calibrated
estimator in `gitai_llm::tokens` rather than a real tokenizer, because the
vocabulary of whatever a local runtime is serving is usually unknown. It is
deliberately pessimistic. Actual spend always comes from the provider's own
`usage` field, never from the estimate. Each stage sizes its context from the
model that will read it, so a 32k worker and a 200k arbiter get different
prompts from the same data.

**Process trees.** A gate command runs through a shell, and killing a shell
does not kill its children. On Unix the child gets its own process group and
the group is signalled. On Windows it is assigned to a Job Object with
`KILL_ON_JOB_CLOSE`, so the kernel guarantees the cleanup even if gitai itself
dies. Relatedly, `cmd /C` mangles inner quotes when arguments go through Rust's
normal quoting, so shell commands are passed verbatim after `/S`.

## Still open

- **The read API has no authentication.** `/api/tasks` and the event stream
  hand out issue titles, plans, diffs and gate output to anyone who can reach
  the port. Only the webhook endpoint authenticates its callers, by signature.
  On a public host, publish `/webhooks/*` and nothing else; both deployment
  guides show how.
- **No GitHub App.** A hosted tier wants one, and installation tokens expire
  hourly with nothing in the forge adapter to refresh them. Machine account
  plus a fine-grained token until then.
- **No polling.** Tasks start from webhooks, so a forge that cannot reach the
  host needs a tunnel.
- **Postgres.** SQLite is right for self-hosted and would not be right for a
  hosted multi-node tier.
- **No repository-wide code search.** The planner sees the file tree and the
  README; the worker asks for files by name. There is no embedding index, so on
  a large unfamiliar repository the planner's `relevant_files` is doing a lot of
  work.
- **No mid-run steering.** A `/gitai` comment starts a task; it cannot redirect
  one that is already running.
- **Cost estimates depend on you.** `price_in` and `price_out` are config, not
  a price list gitai maintains.

## Testing

```bash
cargo test --workspace
```

213 tests, no network, no Docker, no forge. `gitai-pipeline::testing` ships an
in-memory workspace and sandbox, and the `mock` provider answers canned JSON,
so the full engine loop is covered by tests that run in milliseconds. Two of
the bugs above were found by writing those tests rather than by running the
thing.
