# Plugins, bookmarks, workspaces, forwards

## Plugins

Plugins can use inline configuration or separate packages.
To create a package, see [Create a plugin package](plugin-authoring.md).
Packages support named commands, validated inputs, JSON reports, and managed port-forwards.
Enter `:plugin-cancel` to stop the active plugin run.

### Official catalog

The [official catalog](https://github.com/nklmilojevic/sofka-plugins) contains
reviewed plugin packages. Review means a maintainer reviewed that published
version. Plugins still run with your permissions and inherited environment;
the catalog does not provide a process sandbox or guarantee that an adapter or
external tool has no defects.

Catalog commands do not open the TUI, read cluster credentials, connect to
Kubernetes, or execute adapters:

```sh
sofka plugin search [QUERY]
sofka plugin describe ID[@VERSION]
sofka plugin install ID[@VERSION] [ID[@VERSION] ...]
sofka plugin update [ID ...]
sofka plugin list
sofka plugin remove ID [ID ...]
```

`install ID` and `update` select the highest compatible, active, stable package
version. An explicit `ID@VERSION` installs exactly that version and supports
updates and rollbacks. Reinstalling the same intact version succeeds without
changing files. An update run reports every plugin it cannot update — one the
catalog no longer serves, one whose installed version the catalog has dropped,
one with local modifications — updates the rest, and fails at the end. Sofka never updates plugins during startup, search, or
reload.

Search, describe, install, and update fetch the complete `index.json` once per
command and cache its validated commit snapshot. Add `--offline` to use that
snapshot; offline installation also needs the matching cached archive. Sofka
prints the cache age because withdrawal information may be stale. Network
failure does not silently fall back to cached metadata.

Managed packages are installed under
`$XDG_CONFIG_HOME/sofka/plugins/<id>`, or `~/.config/sofka/plugins/<id>`. Each
contains a `.sofka-install.json` record with its versions and file hashes. A
removal moves the package into `sofka/.plugin-trash` before deleting it, and
the next plugin command empties that directory; sofka deletes nothing else in
the config directory, whatever it is named.
`list` works offline and labels manual packages and local modifications. A
symlinked package directory is always manual: sofka runs the package behind the
link and never takes ownership of it. Update and removal refuse modified
packages, symlinks, and unmanaged directories; resolve those paths manually.
There is no destructive force option.

The installer verifies BLAKE3 before extraction, rejects links and unsafe
paths, validates the existing `plugin.toml` format, and activates a complete
staged directory. It also reconciles the staged `plugin.toml` against the
catalog entry the package was selected from and refuses a package whose
version, command, target, output, or safety flags disagree, so what `describe`
reports is what the session runs. It reports missing external tools and their installation
instructions, but does not install them. Metadata and package downloads have
separate budgets: a download that makes no progress for 30 seconds is dropped,
while a large package is given up to 15 minutes to arrive. After an
installation, update, or removal changed anything, enter `:reload` in an
existing session.

Add `--json` to search, describe, and list for stable machine-readable output.
Search returns an array with `id`, `display_name`, `description`, `tags`,
`latest_version`, `compatible`, `installed`, `installed_version`, and
`withdrawal_reason`. Describe returns the catalog and execution fields printed
by the text view, including `installed_withdrawal_reason` when a different
installed release was withdrawn. List returns an array with `id`, `version`,
`path`, `managed`, `modified`, and `withdrawal_reason`.

Home Manager users can continue to place immutable package sources in the same
directory. Sofka reports those as manual packages and does not take ownership
of them. Since `plugin` is now a CLI command, use `sofka --resource plugin` to
open a Kubernetes resource whose name is exactly `plugin`.

sofka ships one plugin: `:sanitize`, which deletes the pods a namespace has
finished with. It needs nothing installed - see [Sanitize pods](../plugins/sanitize/README.md).
An inline entry or a user package of the same name replaces it.

For core plugins maintained and shipped by the sofka project, Rust is the preferred language.
External plugins can use any programming language that follows the plugin protocol.
See [Language choice](plugin-authoring.md#language-choice).

`[[plugins]]` assigns an external command to a key or palette command. `key` is a **chord**: a single
character (`"g"`), a modifier combination (`"ctrl-g"`, `"alt-x"`, `"shift-b"`), or
a function or named key (`"f5"`, `"ctrl-f2"`). A built-in key wins over a plugin
on the same chord. You can [change or disable that built-in binding](keybindings.md)
to release the key. `:config` reports bindings hidden by built-in actions.

For the minus key, use `"-"` alone or `"ctrl--"`, `"alt--"`, or `"ctrl-alt--"`
with modifiers. The final two hyphens are the separator and the minus key.
`"shift--"` is not supported. Bind the character that your keyboard produces
instead. For example, use `"_"` if Shift+minus produces an underscore, or
`"ctrl-_"` to add Ctrl.

```toml
[[plugins]]
key = "shift-y"
name = "yaml-summary"
command = "kubectl"
args = ["get", "$RESOURCE", "$NAME", "-n", "$NAMESPACE", "-o", "yaml"]
scopes = ["pods", "deployments"]   # omit for all resources
mutating = false          # read-only: still runs under --readonly
output = "popup"          # captured into a scrollable view (see below)

[[plugins]]
key = "ctrl-x"
name = "restart-rollout"
command = "kubectl"
args = ["rollout", "restart", "$RESOURCE/$NAME", "-n", "$NAMESPACE"]
scopes = ["deployments"]
dangerous = true          # confirm (showing the exact command) first
```

- **Placeholders** are substituted as whole arguments, never spliced into a shell
  string: `$NAME`, `$NAMESPACE`/`$NS`, `$CONTEXT`, `$CLUSTER`, `$RESOURCE`
  (plural), `$GROUP`, `$VERSION`, `$KIND`, `$FILTER`.
- **`output`** selects `terminal`, `popup`, `background`, or `report`.
  `terminal` is the default. It suspends the TUI for an interactive command.
  `popup` shows captured text. `background` shows a completion message.
  `report` shows a [JSON report](plugin-authoring.md#report-format).
  Captured modes use `timeout` (`"30s"` by default) and enforce output limits.
- **`palette`** assigns a command name, such as `palette = "scan"` for `:scan`.
  The `key` field is optional when `palette` is present.
- **`target = "context"`** runs once without a selected row.
  The default, `selection`, uses selected or marked rows.
- **`requires`** lists required executables. **`install`** supplies instructions when an executable is absent.
- **`inputs`** defines validated `name=value` arguments.
  See [Inputs](plugin-authoring.md#inputs).
- **`network_load = true`** identifies a load test.
  It requires confirmation and blocks the plugin in read-only mode.
- **`mutating`** (default `true`): read-only mode blocks a mutating plugin. Set
  it to `false` to allow a known read-only one.
- **`confirm`** / **`dangerous`**: prompt before running, showing the exact
  executable and arguments. `dangerous` also shows ⚠.
- **`shell = true`**: opt into `sh -c`. Placeholders still arrive as positional
  parameters (`$1`, `$2`, …), never interpolated into the script.
- **Bulk**: with rows marked (`space`), a `popup` or `background` plugin runs over
  every marked row and reports partial failures. An interactive `terminal` plugin
  can't run over a set and refuses a marked run.

Guardrails match plugin actions with `plugin:<palette>`, or `plugin:<name>` when no palette command exists.

On an invalid value (a bad chord, an unknown `output`, a malformed `timeout`)
sofka disables just that plugin or falls back to the default and shows a warning
in `:config`. Plugins appear in `?` help with their chord and scope.

## Bookmarks

`[[bookmarks]]` are saved navigation commands. One keystroke jumps to a resource
and can switch context or namespace and apply a filter, sort, and view. The key
chord is optional - bookmarks are always in the command palette (`★`, ranked
above resources).

```toml
[[bookmarks]]
key = "shift-1"                          # optional
name = "Prod API failures"
resource = "pods"
context = "prod-eu"                      # optional: switched first
namespace = "checkout"                   # optional; all/* = all namespaces
filter = "status!=Running -l app=api"    # optional, same syntax as `/`
sort = "RESTARTS:desc"                   # optional: COLUMN[:asc|:desc]
view = "xray"                            # optional: xray | pulse
```

## Workspaces

Without an active workspace, `Tab` / `Shift-Tab` cycle pods → services →
deployments → statefulsets → daemonsets → secrets → configmaps → ingresses →
PVCs in the current namespace, including all namespaces. The cycle wraps and
skips kinds absent from API discovery. From a resource outside this set, `Tab`
starts at pods and `Shift-Tab` starts at PVCs (or the next available kind in
that direction). Switching resources clears filters and drill-down scope, as
with `:resource`; `[` / `]` still navigate view history.

`[[workspaces]]` group several views into a named set for one task - checkout
ops, a cluster upgrade, cert renewal. Open one with a chord or the palette (`▦`).
sofka switches the optional context once and shows the first view. `Tab` /
`Shift-Tab` cycle the other views. You stay in the workspace.

```toml
[[workspaces]]
key = "ctrl-w"
name = "Checkout ops"
context = "prod-eu"          # optional: switched once on open

[[workspaces.views]]
name = "API pods"
resource = "pods"
namespace = "checkout"
filter = "-l app=api"
sort = "RESTARTS:desc"

[[workspaces.views]]
name = "Ingress"
resource = "ingresses"
namespace = "checkout"
```

## Saved forwards

Port-forwards started with `f`/`F` run in the background and are managed with
`:pf`. `[[forwards]]` adds named entries that appear in `:pf` even while stopped
(`⏎` starts one), with optional `autostart` on connect and on matching context
switches.

```toml
[[forwards]]
name = "argocd"
target = "svc/argocd-server"  # kubectl syntax: pod/…, svc/…, deploy/…
namespace = "argocd"
ports = "8080:443"            # LOCAL:REMOTE
autostart = true              # start when sofka connects (default false)
contexts = ["home"]           # optional: only these contexts
```

sofka stops every forward on quit instead of orphaning it.
