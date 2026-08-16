# Deploying gitai on Linux, with Gitea

End to end: a Debian or Ubuntu box, a self-hosted Gitea, gitai wired to it, and
an issue that turns into a pull request. About forty minutes, most of it
waiting for images to pull.

Every command assumes Debian 12 or Ubuntu 22.04+. Adjust the package manager
and the rest carries over.

---

## 0. What is going where

```
        ┌──────────────────────────────────────────────┐
        │  your Linux host                             │
        │                                              │
        │   Gitea (container, :3000)                   │
        │      │  webhook  →  http://host.docker.internal:8080
        │      ▼                                       │
        │   gitai (host process, :8080) ──┐            │
        │      │                          │ spawns     │
        │      │ git over HTTPS           ▼            │
        │      └───────────►  attempt containers        │
        │                     (--network none)          │
        │                                              │
        └──────────────┬───────────────────────────────┘
                       │ HTTPS
                       ▼
                 model provider API
```

Two decisions are baked into that picture and worth understanding before you
copy commands.

**gitai runs on the host, not in a container.** It bind-mounts each attempt's
checkout into a sandbox container. Bind mount paths are resolved by the Docker
daemon against the *host* filesystem, so if gitai were itself containerised it
would hand the daemon paths that mean nothing. Running it on the host keeps
that simple.

**gitai needs the Docker socket, and that is root-equivalent.** Anyone who can
talk to `/var/run/docker.sock` can mount `/` into a privileged container. Group
membership in `docker` is therefore not a small permission. Give gitai its own
machine if the repositories matter, or at minimum its own unprivileged user
that does nothing else.

---

## 1. Host packages

```bash
sudo apt update
sudo apt install -y git curl ca-certificates
```

Docker, from Docker's own repository rather than the distro's:

```bash
curl -fsSL https://get.docker.com | sudo sh
sudo systemctl enable --now docker
```

Check it:

```bash
docker run --rm hello-world
```

---

## 2. Build gitai

Rust 1.85 or newer:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

```bash
git clone <your gitai repo> ~/src/gitai
cd ~/src/gitai
cargo build --release
```

One static-ish binary comes out at `target/release/gitai`. Put it where the
service will run from:

```bash
sudo useradd --system --create-home --home-dir /opt/gitai --shell /usr/sbin/nologin gitai
sudo usermod -aG docker gitai

sudo install -o gitai -g gitai -m 0755 target/release/gitai /opt/gitai/gitai
sudo -u gitai cp -r prompts /opt/gitai/
```

Copying `prompts/` is optional; the templates are compiled into the binary and
files on disk only override them. Copy them if you intend to tune the prompts,
which you eventually will.

---

## 3. Gitea

```bash
sudo -u gitai mkdir -p /opt/gitai/gitea
sudo -u gitai cp ~/src/gitai/deploy/gitea-compose.yml /opt/gitai/gitea/
cd /opt/gitai/gitea
sudo -u gitai mkdir gitea-data
sudo -u gitai docker compose -f gitea-compose.yml up -d
```

Open `http://<host>:3000`, work through the install page, and create your own
admin account at the bottom of it.

Then turn registration off, since this is not a public forge:

```bash
sudo -u gitai docker compose -f gitea-compose.yml exec gitea \
  sh -c "echo '[service]\nDISABLE_REGISTRATION = true' >> /data/gitea/conf/app.ini"
sudo -u gitai docker compose -f gitea-compose.yml restart gitea
```

### The account gitai acts as

Give it its own login. Everything it does shows up under this name, and it is
also how gitai recognises and ignores its own activity.

```bash
sudo -u gitai docker compose -f gitea-compose.yml exec -u git gitea \
  gitea admin user create \
    --username gitai \
    --password "$(openssl rand -base64 24)" \
    --email gitai@localhost \
    --must-change-password=false
```

Log in as that user once, go to **Settings → Applications → Generate New
Token**, and grant it:

| Scope | Why |
| --- | --- |
| `read:repository`, `write:repository` | clone, push branches, delete them afterwards |
| `read:issue`, `write:issue` | read the issue, comment, manage labels |
| `read:user` | resolve its own identity |

Copy the token. It is shown once.

Finally, add the account as a collaborator with write access on whichever
repository you want it working on.

---

## 4. The sandbox image

The attempt container needs the toolchain your repository builds with, and
nothing else. The fastest route is your existing CI image. Failing that, a
plain language image works:

| Repository | Image |
| --- | --- |
| Rust | `docker.io/library/rust:1-slim` |
| Node | `docker.io/library/node:22-slim` |
| Python | `docker.io/library/python:3.12-slim` |
| Go | `docker.io/library/golang:1-alpine` |

Pull it once so the first task does not pay for it:

```bash
sudo -u gitai docker pull docker.io/library/rust:1-slim
```

Two things about this container that shape how you write `[gate]` below.

**It has no network.** `--network none` is the default and should stay that
way: code written by a model has no business making outbound connections.
Dependency fetching therefore has to happen in `gate.setup`, which runs before
the lockdown.

**It has no git.** git always runs on the host, so the sandbox never holds a
credentialed remote. If your build stamps a version from `git describe`, that
step will fail inside the sandbox and you will need to stub it.

---

## 5. Configuration

```bash
sudo -u gitai /opt/gitai/gitai init /opt/gitai
```

That writes `/opt/gitai/gitai.toml` and the prompt templates. Now edit it. The
sections below are the ones that actually need your attention.

### Secrets

Nothing secret goes in the file. `${VAR}` is expanded from the environment at
load time.

```bash
sudo mkdir -p /etc/gitai
sudo tee /etc/gitai/env >/dev/null <<'EOF'
ANTHROPIC_API_KEY=sk-ant-...
GITEA_TOKEN=<the token from step 3>
GITEA_WEBHOOK_SECRET=<paste the output of: openssl rand -hex 32>
EOF
sudo chmod 600 /etc/gitai/env
sudo chown root:root /etc/gitai/env
```

### Forge

```toml
[forges.local]
kind = "gitea"
# gitai reaches Gitea on the host's published port.
base_url = "http://localhost:3000/api/v1"
token = "${GITEA_TOKEN}"
webhook_secret = "${GITEA_WEBHOOK_SECRET}"
trigger_label = "gitai"
bot_login = "gitai"
draft_prs = true
delete_rejected_branches = true
```

### Sandbox

```toml
[sandbox]
kind = "docker"
image = "docker.io/library/rust:1-slim"
workdir = "/work"
network = "none"
cpus = 4.0
memory = "8g"
timeout_secs = 1800
work_root = "/opt/gitai/work"
```

Leave `user` empty. On Linux gitai reads the owner of `work_root` and passes
`--user uid:gid`, so files written inside the container come back owned by the
gitai account instead of by root.

Toolchain caches are the difference between a thirty-second gate and a
five-minute one. Mount them:

```toml
mounts = [
  "/opt/gitai/cache/cargo:/usr/local/cargo/registry",
  "/opt/gitai/cache/target:/work/target",
]
```

```bash
sudo -u gitai mkdir -p /opt/gitai/cache/{cargo,target} /opt/gitai/work
```

### The gate

This is the part that matters most, and the part no example can write for you.
Put in the commands your repository actually builds and tests with.

```toml
[gate]
setup = ["cargo fetch --locked"]
build = ["cargo build --all-targets --locked"]
test  = ["cargo test --all --locked"]
lint  = ["cargo clippy --all-targets -- -D warnings"]
require_tests = false
enforce_scope = true
max_changed_files = 60
```

An empty `[gate]` means nothing can objectively reject a patch, and the whole
pipeline collapses into models grading each other. `gitai doctor` exits non-zero
rather than let that pass quietly.

### Models

One provider, several models, which is the usual starting point:

```toml
[providers.anthropic]
kind = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
timeout_secs = 300
max_retries = 3
concurrency = 8

[models.big]
provider = "anthropic"
model = "claude-opus-5"
temperature = 0.2
max_tokens = 16000
context_tokens = 200000
# Fill these from the provider's price list, in USD per million tokens.
# Left at zero, cost is always computed as zero and budget.max_cost_usd
# never fires. The token and wall-clock ceilings still work.
price_in = 0.0
price_out = 0.0

[models.mid]
provider = "anthropic"
model = "claude-sonnet-5"
temperature = 0.3
max_tokens = 12000
context_tokens = 200000
price_in = 0.0
price_out = 0.0

[models.small-cautious]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
temperature = 0.3
max_tokens = 8000
context_tokens = 200000
price_in = 0.0
price_out = 0.0

# Same model, different temperature. Two samples from one model at one
# temperature tend to fail the same way, and this is the cheapest diversity
# there is.
[models.small-bold]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
temperature = 0.9
max_tokens = 8000
context_tokens = 200000
price_in = 0.0
price_out = 0.0

[roles]
planner  = "big"
worker   = ["small-cautious", "small-bold"]
editor   = "mid"
reviewer = "mid"
arbiter  = "big"
```

gitai cycles the worker list, so three attempts a round come out as cautious,
bold, cautious.

### Budget

```toml
[budget]
max_rounds = 3
attempts_per_round = 3
max_iterations = 8
max_tokens = 2000000
max_wall_secs = 3600
max_cost_usd = 5.0
```

Start lower than feels right. `max_iterations` is the one that runs away: it is
how long a single attempt is allowed to keep failing the gate and trying again.

### Server

```toml
[server]
bind = "0.0.0.0:8080"
public_url = "http://localhost:8080"
# Tasks at once. Each fans out to attempts_per_round containers, so this
# times that is the real container count. Three tasks times three attempts
# on a four-core box is not going to be a good time.
concurrency = 2

[storage]
url = "sqlite:///opt/gitai/gitai.db?mode=rwc"
```

`bind` has to be reachable from inside the Gitea container, so `0.0.0.0` rather
than `127.0.0.1`. If the host is exposed to anything beyond your LAN, put it
behind a reverse proxy and firewall the port. The only endpoint that
authenticates callers is the webhook, and it does so by signature.

---

## 6. Check before running

```bash
sudo -u gitai env $(sudo cat /etc/gitai/env | xargs) \
  /opt/gitai/gitai --config /opt/gitai/gitai.toml doctor
```

It calls every configured model for real, checks the Docker backend, validates
every cross-reference in the config, and lists what it found. Fix anything it
calls a PROBLEM before going further; warnings are things that will work and
that you should have decided on deliberately.

---

## 7. Run it as a service

```bash
sudo cp ~/src/gitai/deploy/gitai.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now gitai
systemctl status gitai
journalctl -u gitai -f
```

---

## 8. Wire up the webhook

In Gitea, on the repository: **Settings → Webhooks → Add Webhook → Gitea**.

| Field | Value |
| --- | --- |
| Target URL | `http://host.docker.internal:8080/webhooks/local` |
| HTTP Method | `POST` |
| POST Content Type | `application/json` |
| Secret | the same string as `GITEA_WEBHOOK_SECRET` |
| Trigger On | Custom Events: **Issues** and **Issue Comment** |

The path segment after `/webhooks/` is the key of the `[forges.*]` block, so
`local` here matches `[forges.local]` above.

Press **Test Delivery**. A green tick means the signature checked out. A 401
means the secret does not match. A connection error means either
`ALLOWED_HOST_LIST` is not set in the compose file, or gitai is bound to
`127.0.0.1` where the container cannot see it.

Last piece: create a label called `gitai` on the repository. Only issues
carrying it are picked up.

---

## 9. First task

Open an issue. Describe the change the way you would for a new colleague who
knows the language but not the codebase: what is wrong, how to see it, what
done looks like. Then add the `gitai` label.

Watch it work:

```bash
journalctl -u gitai -f
```

or over HTTP:

```bash
curl -s localhost:8080/api/tasks | jq '.tasks[] | {id, state, issue: .issue.title}'
curl -N localhost:8080/api/tasks/<id>/events
```

The second one is a Server-Sent Events stream and closes itself when the task
reaches a terminal state. Pass `?after=<seq>` to resume where you left off.

What you should see: a comment on the issue with the plan, then several
minutes of nothing visible while attempts run in parallel, then a draft pull
request. Every attempt that cleared the gate was pushed as a branch; the ones
that were not selected are deleted when the task ends.

---

## Trying it without spending anything

Before pointing this at a repository you care about, run the whole pipeline
offline. The `mock` provider answers canned JSON, so nothing leaves the machine
and no key is needed:

```toml
[providers.mock]
kind = "mock"
base_url = ""

[models.mock]
provider = "mock"
model = "mock"

[roles]
planner = "mock"
worker = ["mock"]
editor = "mock"
reviewer = "mock"
arbiter = "mock"
```

```bash
git clone http://localhost:3000/you/some-repo /tmp/probe
gitai --config /tmp/mock.toml run --repo /tmp/probe --title "probe" --body "probe"
```

It finishes in about a second and leaves a branch behind. That proves the
wiring: the sandbox, the gate, the queue, the event log. It proves nothing at
all about the models.

---

## GitHub instead of, or alongside, Gitea

Everything above still applies. What changes is that **GitHub delivers webhooks
to you**, so your host has to be reachable, and that a personal access token
acts as the person who created it, which has a trap in it.

That is its own guide: **[deploy-github.md](deploy-github.md)**.

Both forges can be configured at once. They get separate endpoints, separate
tokens, and their tasks share one queue.

---

## When something goes wrong

**The webhook fires and nothing happens.** The issue is missing the
`trigger_label`, or it was opened by `bot_login` and gitai skipped its own
noise. Gitea's webhook page shows the request and the response body, which
names the reason.

**Every attempt fails the gate on `setup`.** Almost always the missing network.
`gate.setup` runs before the lockdown, but if your install step itself shells
out to something that needs network *later*, it will fail. Bake dependencies
into the sandbox image instead.

**Attempts fail with permission errors on files.** `work_root` is owned by
someone other than the gitai user, so the derived `--user` is wrong.
`chown -R gitai:gitai /opt/gitai/work`.

**Tasks sit in `queued` forever.** The runners are not running. `journalctl -u
gitai` will show why, usually a Docker permission problem: the gitai user is
not in the `docker` group, or was added without re-logging in. The service
picks up group changes on restart.

**A run costs far more than expected.** `price_in` and `price_out` are still
zero, so `max_cost_usd` never fired. Fill them in, or lower `max_tokens`, which
works regardless.

**Everything is slow.** Check whether the cache mounts are actually being used:
`docker inspect` an attempt container mid-run. Without them every attempt
rebuilds the world from scratch.
