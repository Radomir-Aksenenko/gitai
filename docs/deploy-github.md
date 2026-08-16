# Connecting gitai to github.com

Everything in [deploy-linux.md](deploy-linux.md) still applies: the host
packages, the Docker sandbox, the gate, the models, the systemd unit. This
document only covers what is different when the forge is public GitHub rather
than a Gitea you control.

Three things are different, and the first one decides your whole setup.

---

## 1. GitHub delivers webhooks to you

With a self-hosted Gitea both halves sit on the same network and nothing has to
be exposed. GitHub is outside, and it has to be able to open an HTTP connection
to your machine. A laptop behind NAT cannot receive that.

### Option A: a VPS with a public address (what to do if this stays)

The ordinary answer. gitai on a small server, a domain pointed at it, TLS in
front.

Bind gitai to loopback only:

```toml
[server]
bind = "127.0.0.1:8080"
public_url = "https://gitai.example.com"
```

Then put Caddy in front. This config is the whole thing, TLS certificates
included:

```caddy
gitai.example.com {
    # Only the webhook path is reachable from outside. See section 4 for why
    # this matters more than it looks.
    @webhooks path /webhooks/*
    handle @webhooks {
        reverse_proxy 127.0.0.1:8080
    }

    handle {
        respond "not found" 404
    }
}
```

```bash
sudo apt install -y caddy
sudo systemctl restart caddy
```

Your webhook URL is then `https://gitai.example.com/webhooks/hub`.

### Option B: a Cloudflare Tunnel (no public address needed)

Works from a machine with no inbound ports open at all, including a home
server behind NAT. Free, and it survives restarts once installed as a service.

```bash
curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64.deb -o /tmp/cf.deb
sudo dpkg -i /tmp/cf.deb

cloudflared tunnel login
cloudflared tunnel create gitai
cloudflared tunnel route dns gitai gitai.example.com
```

`/etc/cloudflared/config.yml`:

```yaml
tunnel: gitai
credentials-file: /root/.cloudflared/<tunnel-id>.json

ingress:
  - hostname: gitai.example.com
    path: /webhooks/.*
    service: http://127.0.0.1:8080
  # Everything else is refused at the edge rather than forwarded.
  - service: http_status:404
```

```bash
sudo cloudflared service install
sudo systemctl enable --now cloudflared
```

### Option C: ngrok

```bash
ngrok http 8080
```

Fine for an afternoon of testing. The URL changes on every restart on the free
plan, which means re-editing the webhook each time, so do not build a habit on
it.

---

## 2. Identity: use a machine account

A personal access token acts as **the user who created it**. This has a
consequence people trip over:

> gitai ignores issues opened by `bot_login`, so that its own activity does not
> restart the loop. If you use your own token and set `bot_login` to your own
> username, gitai will silently ignore every issue you open.

So: create a separate GitHub account for the bot. Free, takes two minutes, and
it also means the commits and comments are visibly not yours.

1. Sign up a new account, for example `yourorg-gitai`.
2. Invite it as a collaborator with **Write** access on the repository, or add
   it to the organisation.
3. Log in as that account and create the token.

### The token

**Settings → Developer settings → Personal access tokens → Fine-grained
tokens**. Scope it to only the repositories gitai should touch.

| Permission | Access | Why |
| --- | --- | --- |
| Contents | Read and write | clone, push attempt branches, delete them after |
| Issues | Read and write | read the issue, comment, manage labels |
| Pull requests | Read and write | open the pull request |
| Metadata | Read | mandatory, granted automatically |

A classic token with the `repo` scope also works and is broader than necessary.

**On GitHub Apps.** An App would be the right shape for a hosted product: per
installation permissions, no personal account in the loop, higher rate limits.
gitai does not support one yet, because App installation tokens expire every
hour and nothing in the forge adapter refreshes them. That is a known piece of
work, not an oversight. For now, a token on a machine account.

---

## 3. Configuration

```toml
[forges.hub]
kind = "github"
base_url = "https://api.github.com"
token = "${GITHUB_TOKEN}"
webhook_secret = "${GITHUB_WEBHOOK_SECRET}"
trigger_label = "gitai"
# The machine account, not you.
bot_login = "yourorg-gitai"
draft_prs = true
delete_rejected_branches = true
timeout_secs = 30
```

Secrets into `/etc/gitai/env`, mode 0600:

```bash
GITHUB_TOKEN=github_pat_...
GITHUB_WEBHOOK_SECRET=<openssl rand -hex 32>
```

GitHub Enterprise Server uses the same adapter with a different root:

```toml
base_url = "https://ghe.yourcompany.com/api/v3"
```

Both forges can be configured side by side. They get separate endpoints
(`/webhooks/hub`, `/webhooks/local`), separate tokens, and their tasks share
one queue.

### The webhook on GitHub

Repository → **Settings → Webhooks → Add webhook**.

| Field | Value |
| --- | --- |
| Payload URL | `https://gitai.example.com/webhooks/hub` |
| Content type | `application/json` |
| Secret | the same string as `GITHUB_WEBHOOK_SECRET` |
| SSL verification | Enable |
| Events | Let me select individual events: **Issues**, **Issue comments** |

The path segment after `/webhooks/` is the key of the `[forges.*]` block, so
`hub` matches `[forges.hub]`.

GitHub sends a `ping` on save. gitai verifies its signature and answers 200 with
`{"status":"ignored"}`, which is the correct result: the delivery was authentic
and there was nothing to do with it. **Recent Deliveries** on the webhook page
shows the request and the response body, and it is the first place to look when
something does not fire.

Last: create a label named `gitai` on the repository. Only issues carrying it
are picked up.

---

## 4. What is now exposed, and what to do about it

Your webhook endpoint is a public URL that starts model runs costing real
money. Two things protect it, and you should understand both.

**The signature.** Every delivery carries `X-Hub-Signature-256`, an HMAC of the
body under your secret. gitai verifies it in constant time before it parses the
body, and refuses to serve a forge whose `webhook_secret` is empty rather than
treating that as "no check needed". Use a long random secret and it holds.

**Nothing else.** The read API (`/api/tasks`, the event stream) has **no
authentication at all**. It will happily hand out issue titles, plans, diffs
and gate output to anyone who asks. On a private network that is fine. On a
public address it is a leak, which is why the Caddy and Cloudflare configs
above publish `/webhooks/*` and nothing else, and why `bind` is on loopback.

If you want the API reachable too, put basic auth or an IP allow-list in front
of it in the proxy. Do not simply open the port.

Optional extra: GitHub publishes its webhook source ranges at
`https://api.github.com/meta` (`hooks`). Restricting inbound to those in your
firewall costs nothing and cuts the noise.

---

## 5. Things that behave differently on GitHub

**Draft pull requests are real.** Gitea fakes them with a `WIP:` title prefix;
GitHub has an actual `draft` flag and gitai uses it. `draft_prs = true` means
CI does not fire until a human marks it ready.

**Branch protection does not get in the way.** gitai only ever pushes to
`gitai/issue-*` branches and opens a pull request against the base. Protected
base branches are not touched. If you require status checks, they will run
against the PR as usual and the human sees the result before merging.

**Rate limits are generous for this workload.** A fine-grained token gets 5000
API requests an hour. A task uses on the order of ten. Cloning is not counted
against that limit.

**Private repositories work unchanged.** The clone URL gitai builds is
`https://x-access-token:<token>@github.com/owner/repo.git`, which authenticates
for private repositories given the Contents permission. That URL is treated as
a secret throughout: it never reaches a log, an event payload, or a model, and
git's own error messages are scrubbed of credentials before they are recorded.

**Actions minutes.** Every attempt that clears the gate is pushed as a branch.
If you have workflows triggered `on: push`, that is several branch pushes per
task, each potentially burning minutes. Either scope those workflows with a
`branches-ignore: ['gitai/**']`, or accept the cost. The branches that lose are
deleted when the task ends, but the workflow runs they triggered are not.

---

## 6. First run

```bash
sudo systemctl restart gitai
journalctl -u gitai -f
```

Open an issue, add the `gitai` label, and watch. Expect a comment with the plan
within a minute, then quiet while attempts run, then a draft pull request.

If nothing happens, the order to check is: **Recent Deliveries** on the GitHub
webhook page (did it arrive, what did gitai answer), then `journalctl -u gitai`
(did it get queued), then `curl localhost:8080/api/tasks` (what state is it in).
