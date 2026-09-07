# Debugging and incident workflow

## Explain unhealthy (`X`)

A deterministic, evidence-based answer to "why is this broken?" for the selected
object: rollout state, degraded conditions, the blocking pods and their container
failure reasons (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable,
failed probes), and recent Warning events. No AI, no external service.

`j`/`k` move, `⏎` goes to the resource behind a finding, `E` its events, `l` its
logs, `r` gathers again. A finding you can drill into has a trailing `→`.

For Nodes, memory, disk, and PID pressure are warnings when their conditions
are `True`. `NetworkUnavailable=True` is also a warning. These conditions do
not produce warnings when they are `False`. `Unknown` remains a warning, and
the `Ready` condition is assessed separately.

The DaemonSet rollout summary reads available pods from `status.numberAvailable`.

## Timeline (`T`)

A per-object timestamped log of every state change the watch saw this session:
generation bumps, replica and readiness changes, pod phase, restarts, waiting
reasons, condition flips. Diffed from the watch stream, bounded in size, never
written to disk.

Restart history includes normal init containers and native sidecars. It keeps
completed init restart counts in its total, so the end of initialization does
not reset the count. The pod table excludes normal init restarts after
initialization is complete.

## Diff (`:diff`)

A unified diff of the live object against its `last-applied-configuration`. When
that annotation is missing - as it is for every Flux-, ArgoCD-, or Helm-managed object,
which nothing ever `kubectl apply`s - sofka diffs against the previous revision
this session's watch saw instead, so "what just changed?" has an answer on GitOps
clusters. The last revision of up to 256 changed objects is kept in memory.

## Notifications

`:notify` toggles a notification on the selected object. Sophie watches it so you
don't have to: every state change the watch sees (the same transitions the
timeline records - rollout progress, readiness, phase, restarts, waiting reasons,
conditions) flashes in the status line, rings the terminal bell, and fires a
**desktop notification**.

Each notify is its own bounded single-object watch, so it keeps firing while you
browse other views - "tell me when this rollout finishes" and keep working.
`:notify` on the same row turns it off. Everything is session-local.

```toml
[notify]
bell = true         # ring the terminal bell
desktop = "osc777"  # "osc777" | "osc9" | "both" | "off"
# command = ["notify-send", "sofka", "$MESSAGE"]     # Linux, inside tmux
# command = ["terminal-notifier", "-title", "sofka"] # macOS ($MESSAGE appended)
```

- `osc777` (default) - rxvt-style title+body, the form Ghostty recommends. Also
  kitty, WezTerm, foot, urxvt.
- `osc9` - iTerm2-style body-only, for iTerm2 and Windows Terminal, which speak
  only that.
- `both` and `off` are also valid. Terminals ignore protocols they don't speak.

Inside a **terminal multiplexer**, which swallows escape sequences from its panes,
set `command` to run a local notifier subprocess instead (`$MESSAGE` is
substituted as a whole argument, never through a shell).

In a **herdr** pane no config is needed at all: sofka detects the pane
environment and delivers through `herdr notification show`, so the toast follows
herdr's own `ui.toast` delivery (in-app, outer terminal, or system).

## Log controls

The kubelet logs view (`l`) keeps a bounded follow buffer. Tune the initial tail,
the buffer size, and an optional `since` lookback:

```toml
[logs]
tail = 300         # initial lines fetched per stream (kubectl --tail)
buffer = 5000      # max lines kept while following (oldest dropped)
since = "1h"       # optional: only logs newer than this, within the tail limit
fullscreen = false # open log views fullscreen (F toggles per session)
```

The `since` window and the `1`–`5` time anchors keep the initial line limit.
A pod stream requests at most `tail` initial lines per container. Workload and
Service streams request at most `min(tail, 100)` initial lines per container.
The time window can reduce this number. Live following continues after these
initial lines. Previous-container logs keep their full history.

In the view, `/` filters with a case-insensitive substring, a `/regex/`, or a
leading `!` to invert (keep lines that don't match). A malformed regex is flagged
instead of hiding everything. `z` clears the on-screen buffer while the live
stream keeps appending. A pod streams every container's logs at once. Full keymap:
[Logs view](keys.md#logs-view).

For history that outlives the pod, use [VictoriaLogs](providers.md#log-provider-victorialogs).

## Debug containers and pods

`:debug` on a **pod** attaches a temporary ephemeral debug container with
`kubectl debug`. sofka prompts for the image (prefilled from `[debug]`). An empty
`command` starts an interactive shell (bash if the image has it, else sh), like
the pod shell. `d` in the container picker sets `--target=<container>` so the
debug container shares that container's process namespace. The ephemeral
container stays on the pod until the pod is recreated - Kubernetes can't remove
it, so there's nothing for sofka to clean up.

`:debug` on a **node** starts a privileged diagnostic pod on it
(`kubectl debug node/<node>`, image `node_image` in `node_namespace`, optional
`node_profile`). That pod mounts the host filesystem at `/host` and joins the host
PID, network, and IPC namespaces, so sofka previews exactly that access and makes
you confirm before creating it. sofka records the node debuggers it started this
session and `:debug-clean` deletes them (matched by the `node-debugger-*` name and
the node). kubectl leaves the pod behind after you exit, so clean up when you're
done.

```toml
[debug]
image = "nicolaka/netshoot:latest"       # ephemeral (in-pod) debug image
command = ["bash"]                       # entrypoint; omit for an interactive shell
node_image = "nicolaka/netshoot:latest"  # node debug pod image
node_namespace = "default"               # namespace the node debugger lands in
node_profile = "sysadmin"                # kubectl debug --profile (optional)
```

Read-only mode and [guardrails](safety.md#guardrails) gate both actions: the
`debug` action for pods, `node-debug` for nodes. Both are recorded in the
[journal](safety.md#action-journal).

## Diagnostic bundles

`:bundle` assembles a redacted incident bundle for the selected object - its YAML,
the owner, the incident explanation, recent events, the session timeline, bounded
recent logs, and a metrics snapshot - into one Markdown document. It's for handing
an incident between application and platform teams. sofka gathers it off-thread
and shows a preview, then `:bundle-save` writes it to a temp file.

Always redacted: Secret `data`/`stringData` values, any credential-like
annotation (a key containing `token`, `password`, `secret`, `apikey`,
`credential`, and similar), and `last-applied-configuration`, all replaced with a
placeholder. `managedFields` is dropped. Env vars sourced from Secrets are flagged
(their values are references, not literals). Every bundle carries a manifest of
exactly what it includes and what it withholds.

```toml
[bundle]
anonymize = false   # replace context/cluster identity with placeholders
log_lines = 200     # max recent log lines per pod
max_pods = 3        # cap how many pods contribute logs
```

## Snapshots

`:snapshot` captures the current table view - its columns and visible rows, plus
metadata (context, cluster, namespace, resource, filter, timestamp) - to a file.
An optional argument sets the format: `text` (default, an aligned table with a
header block), `json`, or `yaml`. Files land in
`$XDG_STATE_HOME/sofka/snapshots` (or `~/.local/state/sofka/snapshots`).

`:snapshots` browses saved captures, newest first with their age. `⏎` opens one in
a viewer with a staleness banner (it's a point-in-time capture), `d` deletes the
highlighted file.

This is not the one-frame `--snapshot` CI flag - this is an interactive
capture-and-review workflow.

## Runtime diagnostics

`:info` shows the version and build, config sources, live context/cluster/API
server and Kubernetes revision, discovery and Metrics API status, watch error
counts, and the state/snapshot/bundle directories. `sofka --info` prints the
static subset without connecting to a cluster. Identifiers and counts only,
never credentials, tokens, or Secret values.

If a kind is missing, check the discovery line first. When sofka cannot read an
API group, it shows a warning at startup:
`warning: API discovery could not read <group>/<version>: <reason>`. `:info`
shows the same warnings under the cluster discovery status. `sofka --check`
also prints the warnings and the number of API groups that sofka did not read.
Sofka cannot skip the core API group. If it cannot read `v1`, the connection
fails with the reason. If aggregated discovery fails, sofka shows the reason
and reads each API group separately.

## X.509 v1 client certificates

Some MicroK8s kubeconfigs contain an X.509 v1 client certificate. The standard
rustls client certificate loader rejects this format. Sofka identifies this
failure as a client certificate error and rejects the connection by default.

To allow this format for one run, pass the explicit flag:

```sh
sofka --allow-v1-client-cert
sofka --allow-v1-client-cert --check
sofka --allow-v1-client-cert --context microk8s
```

The flag applies to context switches, fleet connections, and bundled plugin
adapters in that run. It is not saved to configuration and does not change
kubeconfig. It supports static `client-certificate-data` / `client-key-data`
and `client-certificate` / `client-key` files. It does not support v1
certificates returned by exec credential plugins. Exec plugins with supported
certificates continue to use the standard client path.

Sofka checks that the v1 certificate matches its private key. The flag does not
disable server certificate verification or change TLS versions and ciphers.
Existing kubeconfig trust settings still apply. X.509 v1 is a certificate
format, not TLS 1.0. V1 certificates cannot contain usage restrictions such as
an extended key usage for client authentication. The API server still decides
whether to accept the client certificate.

To check an inline client certificate for the selected context:

```sh
kubectl config view --raw --minify -o jsonpath='{.users[0].user.client-certificate-data}' \
  | openssl base64 -d -A | openssl x509 -noout -text
```

For a certificate file, use `openssl x509 -in client.crt -noout -text`.
`Version: 1 (0x0)` identifies a v1 certificate. To remove the need for the
flag, have the cluster administrator issue a v3 client certificate and update
your kubeconfig. Keep the existing identity and required permissions.

## Teleport local Kubernetes proxy certificates

`tsh proxy kube` can serve a CA certificate as its server certificate. Some TLS
clients reject this with `CaUsedAsEndEntity`, even when the kubeconfig trusts
that exact certificate.

Sofka accepts this setup when the server certificate exactly matches a CA
certificate in the selected kubeconfig's `certificate-authority` file or
`certificate-authority-data`. No extra flag is required. This also applies when
you change contexts or use fleet mode.

Sofka still checks the certificate dates, hostname (including `tls-server-name`),
allowed usage, and TLS signatures. A different certificate with the same key or
subject does not qualify for this exception. Certificates with name constraints
or unsupported critical extensions do not qualify either. Other server
certificates use standard verification. System trust and in-cluster CA file
reloads continue to use the kube client verifier.

Kubeconfigs with exec credential plugins or `auth-provider` entries also keep
standard verification. The CA server certificate exception is not enabled for
these configurations. This avoids extra credential-plugin calls during client
construction. The static certificates generated by `tsh proxy kube` do not have
this limit.

The `--allow-v1-client-cert` flag is separate. It controls the format of the
client certificate and is not needed for a Teleport CA server certificate.
