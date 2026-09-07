//! PVC explore — reading and transferring the contents of a
//! PersistentVolumeClaim.
//!
//! A PVC has no API of its own to read: the only way to see what is on a
//! volume is from inside a pod that mounts it. This module is the pure half of
//! that — picking the pod, describing the helper pod for a claim nothing
//! mounts, parsing the directory listing that comes back, and reading the
//! local side of the split view. Everything that talks to a cluster or the UI
//! lives in `app/pvcexplore.rs`.

use std::path::Path;

use kube::core::DynamicObject;
use serde_json::{Value, json};

/// Where the helper pod mounts the claim. Fixed rather than configurable: it
/// only ever exists inside a pod sofka created, and a knob here would only
/// change a path nobody types.
pub const HELPER_MOUNT: &str = "/pvc";

/// `metadata.generateName` for helper pods. `:pvc-clean` requires this prefix,
/// both of [`HELPER_LABELS`], and the [`HELPER_ANNOTATION`] naming the claim
/// before it deletes anything.
///
/// None of that is unforgeable — every label and annotation sofka writes on
/// creation, anything else can write too — so it is not a permission check.
/// It is there to make an *accidental* match essentially impossible; a
/// deliberate one is bounded instead by the sweep being confirmed, journalled,
/// blocked in read-only mode, and gated by the `pvc-explore` guardrail.
pub const HELPER_PREFIX: &str = "sofka-pvc-explore-";

/// Label every helper pod carries, so a leftover is identifiable as sofka's
/// even after the annotation naming the claim is gone.
pub const HELPER_LABELS: [(&str, &str); 2] = [
    ("app.kubernetes.io/managed-by", "sofka"),
    ("sofka.dev/component", "pvc-explore"),
];

/// Annotation naming the claim a helper pod was created for. Also part of the
/// evidence [`HELPER_PREFIX`] describes.
pub const HELPER_ANNOTATION: &str = "sofka.dev/pvc";

/// The label selector `:pvc-clean` lists with.
pub fn helper_selector() -> String {
    HELPER_LABELS
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Cap on entries read from one directory. A volume with a million files in
/// one directory is a real thing (a cache, a spool), and the cap is applied by
/// `head` inside the container rather than after the fact, so neither the pipe
/// nor this process ever holds the whole listing.
pub const MAX_ENTRIES: usize = 5_000;

/// Prefix of the line `ls`'s own exit status is reported on, since piping
/// through `head` replaces the pipeline's status with `head`'s.
///
/// The prefix alone is forgeable: GNU `ls` prints file names raw when its
/// output is a pipe, so a file called `x\nsofka-ls-status:0` puts a line
/// through that looks exactly like the real marker — and, arriving before the
/// real one, would let a truncated listing pass itself off as complete. Every
/// run therefore mints a nonce (see [`ListingProbe`]) that a name on the
/// volume cannot predict.
const STATUS_MARKER: &str = "sofka-ls-status:";

/// Exit code the listing script uses for a path it could not enter.
pub const EXIT_NOT_A_DIRECTORY: i32 = 3;

/// Exit code the listing script uses for a path that resolved outside the
/// mount. Only a symlink can do that — the browser never builds such a path
/// itself — but a volume's contents are not sofka's to trust.
pub const EXIT_OUTSIDE_MOUNT: i32 = 4;

/// One listing command and the nonce needed to read its output back.
#[derive(Debug, Clone)]
pub struct ListingProbe {
    pub script: String,
    pub nonce: String,
    /// The mount root the listing may not leave, passed as `$2`. The script
    /// resolves it itself, so it is the raw mount path.
    pub root: String,
}

/// A value a file name on the volume cannot guess: the nanosecond clock plus a
/// per-process counter, hex-encoded so it is safe to embed in the script
/// unquoted. It never leaves the exec, so it needs to be unpredictable, not
/// cryptographically random.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    format!("{t:016x}{n:x}")
}

/// The listing command, run as `sh -c <script> sh <path>` so the path arrives
/// as a positional parameter and is never spliced into the script.
///
/// `-A` includes dotfiles but not `.`/`..`; `-l` is the only long format both
/// GNU coreutils and busybox agree on. `LC_ALL=C` pins the month names — not
/// that [`parse_listing`] reads them, but a stable width is one less thing to
/// go wrong.
///
/// The `unset` line matters more than it looks: GNU `ls` reshapes its output
/// from the environment, and the container's environment is not sofka's to
/// choose. `TIME_STYLE=long-iso` prints a two-field date instead of three and
/// shifts the name column, leaving nothing parseable; `QUOTING_STYLE=shell`
/// wraps every name in quotes, so each one lists but none of them resolves;
/// `BLOCK_SIZE`/`LS_BLOCK_SIZE` scale the size column, which would report a
/// 100 kB file as `1B`. busybox ignores all four, and `unset` on an unset name
/// is free, so this is unconditional.
///
/// A failed `cd` exits 3 so the caller can say "not a directory" rather than
/// surface a shell error.
///
/// The cap is applied by `head` inside the container, so a spool directory
/// with a million files is never streamed out in full. That costs the
/// pipeline's exit status — it becomes `head`'s — which matters because `ls`
/// distinguishes three outcomes the pane must not confuse: everything listed
/// (0), listed but some entry could not be stat'd (non-zero, output still
/// good), and could not read the directory at all (non-zero, no output).
/// Hence the trailing marker: it carries `ls`'s real status, and its *absence*
/// means `head` cut the output short, which is exactly the truncation signal.
/// `$1` is the directory to list and `$2` the mount root it may not leave;
/// both arrive as positional parameters, never spliced into the script.
///
/// The `pwd -P` check is what makes "confined to the mount" true rather than
/// aspirational. Path arithmetic alone cannot enforce it: `cd` follows
/// symlinks, so a link on the volume pointing at `/` would land the browser in
/// the serving pod's root with every path still looking like it was under the
/// mount. Comparing the *resolved* directory is the only check that sees it.
///
/// Both sides get a trailing slash before they are compared, which makes the
/// boundary explicit — `/pvcx` is not inside `/pvc` — and keeps a root of `/`
/// working without depending on a glob subtlety to say so.
pub fn list_probe(root: &str) -> ListingProbe {
    let nonce = nonce();
    ListingProbe {
        // Both sides are resolved with `pwd -P` before they are compared: a
        // mount path can itself sit behind a symlink (as `/tmp` does on
        // macOS), and comparing the raw strings would then refuse the mount's
        // own root. Trimming a trailing slash keeps the `"$root"/*` pattern
        // meaningful when the root is `/` — not a legal mountPath, but the
        // script must not misbehave if one ever arrives.
        script: format!(
            r#"[ -n "$1" ] && [ -n "$2" ] || exit {EXIT_NOT_A_DIRECTORY}
unset TIME_STYLE QUOTING_STYLE BLOCK_SIZE LS_BLOCK_SIZE
root=$(cd -- "$2" 2>/dev/null && pwd -P) || exit {EXIT_NOT_A_DIRECTORY}
cd -- "$1" 2>/dev/null || exit {EXIT_NOT_A_DIRECTORY}
case "$(pwd -P)/" in "${{root%/}}/"*) ;; *) exit {EXIT_OUTSIDE_MOUNT} ;; esac
{{ LC_ALL=C ls -A -l; echo "{STATUS_MARKER}{nonce}:$?"; }} | head -n {}"#,
            MAX_ENTRIES + 2
        ),
        nonce,
        root: root.to_string(),
    }
}

/// What one [`list_probe`] run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Body lines that produced no entry. A few are ordinary — a forged row,
    /// a device node in an unfamiliar shape. *All* of them means the output
    /// was not the format we parse, and calling that an empty directory would
    /// tell the user their volume holds nothing.
    pub unparsed: usize,
    /// Rows `ls` produced with no name of their own. A file named `"\nfoo"`
    /// prints its metadata, then the newline ends the row before the name
    /// starts — so this row is the *head*, and `foo` arrives on the next line
    /// as an unparseable one. They are real files sofka cannot name, which is
    /// neither an unreadable listing nor an empty directory.
    pub unnameable: usize,
    /// `head` cut the output short: there are more entries than [`MAX_ENTRIES`].
    pub truncated: bool,
    /// `ls`'s own exit status. `None` when the marker never arrived — either
    /// the output was truncated, or the command never ran at all.
    pub status: Option<i32>,
}

/// Turn one run of [`list_probe`] into either a listing or the reason there
/// isn't one. Pure, so every branch is testable without a cluster: `exit_code`
/// is the process's, `stdout`/`stderr` its output.
pub fn interpret_listing(
    nonce: &str,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<(Listing, Option<String>), String> {
    if exit_code == Some(EXIT_NOT_A_DIRECTORY) {
        return Err("not a directory, or permission denied".into());
    }
    if exit_code == Some(EXIT_OUTSIDE_MOUNT) {
        return Err(
            "that link points outside the volume — the browser stays inside the mount".into(),
        );
    }
    let listing = parse_output(nonce, stdout);
    let message = last_error_line(stderr);
    let fail = |m: String| {
        Err(if m.is_empty() {
            match exit_code {
                Some(c) => format!("listing failed (exit {c})"),
                None => "listing failed".into(),
            }
        } else {
            m
        })
    };
    match listing.status {
        // Truncated: `ls` was still producing output when `head` closed the
        // pipe, so it plainly read the directory — but "plenty of output, none
        // of it parseable" is still unreadable, however much of it there was.
        None if listing.truncated && !listing.entries.is_empty() => Ok((listing, None)),
        None if listing.truncated => Err(unreadable(listing.unparsed)),
        // No marker and no truncation: the command never got as far as
        // reporting a status — a missing shell, a pod that isn't running, a
        // denied exec. Whatever stderr says is the real answer.
        None => fail(message),
        // `ls` succeeded and had something to say, but none of it parsed:
        // some other `ls`, or one whose columns the environment reshaped.
        // Files whose names contain a newline: they are there, and no path
        // sofka builds could reach them. Saying so beats an empty pane — and
        // beats "unreadable output", because each such name also leaves its
        // remainder behind as an unparseable line. That accounting is what
        // tells the two apart: at most one leftover per unnameable row means
        // these are newlines in names, more than that means the format itself
        // is one we do not read.
        Some(0) if listing.unnameable > 0 && listing.unparsed <= listing.unnameable => {
            let n = listing.unnameable;
            Ok((listing, Some(unnameable_note(n))))
        }
        Some(0) if listing.entries.is_empty() && listing.unparsed > 0 => {
            Err(unreadable(listing.unparsed))
        }
        Some(0) => Ok((listing, None)),
        // `ls` failed but still listed entries: it could not stat some of
        // them. Show what there is and say why it is incomplete.
        Some(_) if !listing.entries.is_empty() => {
            Ok((listing, (!message.is_empty()).then_some(message)))
        }
        // `ls` failed with nothing to show — an unreadable directory. This is
        // the case that must never render as "empty".
        Some(_) => fail(message),
    }
}

fn unnameable_note(n: usize) -> String {
    format!(
        "{n} entr{} here contain a newline in the name and cannot be opened or copied",
        if n == 1 { "y" } else { "ies" }
    )
}

fn unreadable(lines: usize) -> String {
    format!("could not read this listing — {lines} lines of unexpected `ls` output")
}

/// The last line of stderr that says something. The real cause comes before
/// kubectl's generic "command terminated with exit code" trailer.
fn last_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| !l.is_empty() && !l.starts_with("command terminated"))
        .unwrap_or_default()
        .to_string()
}

fn parse_output(nonce: &str, stdout: &str) -> Listing {
    let marker = format!("{STATUS_MARKER}{nonce}:");
    let mut status = None;
    let mut body = String::with_capacity(stdout.len());
    let mut lines = 0usize;
    for line in stdout.lines() {
        lines += 1;
        match line.strip_prefix(marker.as_str()) {
            Some(code) => status = Some(code.trim().parse().unwrap_or(-1)),
            None => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    // Counted in *lines*, not entries: a file name containing a newline is
    // several lines and one entry (or none), so counting entries would call a
    // truncated listing complete. `head` emits at most MAX_ENTRIES + 2, so
    // reaching that with no marker means it cut the output short.
    let truncated = status.is_none() && lines >= MAX_ENTRIES + 2;
    let (mut entries, unparsed, unnameable) = parse_body(&body);
    entries.truncate(MAX_ENTRIES);
    Listing {
        entries,
        unparsed,
        unnameable,
        truncated,
        status,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A symlink. Kept distinct from the two above because `ls -l` reports the
    /// link's own size and type, not the target's — descending into one is
    /// something we try, not something we can promise.
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// `None` when nothing could stat the entry — distinct from an empty
    /// file, which the pane would otherwise render identically.
    pub size: Option<u64>,
    /// The `-> target` tail of a symlink line, for display. Empty otherwise.
    pub link_target: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    /// Whether the name survived the round trip intact. `ls` output is decoded
    /// lossily, so a name that is not valid UTF-8 comes back with replacement
    /// characters — it lists, but no path built from it would resolve, so
    /// descending into it or copying it can only fail confusingly.
    pub fn addressable(&self) -> bool {
        !self.name.contains('\u{FFFD}')
    }
}

/// Whether `ls` could plausibly have produced this name. A directory entry can
/// never contain `/` and is never `.` or `..`, so anything that does is a
/// forgery: GNU `ls` writes names raw into a pipe, which lets a file called
/// `x\ndrwxr-xr-x 2 root root 4096 Jan 1 00:00 ..` inject a whole extra row —
/// and a symlink *target* is arbitrary bytes, so it can carry an absolute
/// path. Both would otherwise escape the mount on `enter` and, on download,
/// escape the destination directory (`Path::join` with an absolute path
/// replaces rather than appends).
fn plausible_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && name != "." && name != ".."
}

/// The container to exec into, and where the claim is mounted inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub pod: String,
    pub container: String,
    pub path: String,
    /// The mount is `readOnly` in the pod spec: writes will fail, so an upload
    /// is refused up front instead of failing halfway through a `kubectl cp`.
    pub read_only: bool,
    /// sofka created this pod and owns deleting it.
    pub helper: bool,
}

/// Pick the pod to browse `claim` through: a running pod that already mounts
/// it. Prefers a writable mount over a read-only one, so uploads work whenever
/// any consumer could do them at all.
///
/// `None` means nothing running mounts the claim — the caller's cue to offer a
/// helper pod ([`helper_pod`]).
pub fn find_mount(pods: &[DynamicObject], claim: &str) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for pod in pods {
        if phase(pod) != "Running" {
            continue;
        }
        // A pod on its way out will take the exec with it, and its volume is
        // about to be released.
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let Some(mount) = mount_in(pod, claim) else {
            continue;
        };
        if !mount.read_only {
            return Some(mount);
        }
        best.get_or_insert(mount);
    }
    best
}

/// Names of the pod's containers that are in the `running` state right now,
/// across all three kinds. Anything else cannot be exec'd into.
fn running_containers(pod: &DynamicObject) -> std::collections::HashSet<&str> {
    let Some(status) = pod.data.get("status") else {
        return std::collections::HashSet::new();
    };
    [
        "containerStatuses",
        "initContainerStatuses",
        "ephemeralContainerStatuses",
    ]
    .iter()
    .filter_map(|k| status.get(k))
    .filter_map(Value::as_array)
    .flatten()
    .filter(|c| c.get("state").is_some_and(|s| s.get("running").is_some()))
    .filter_map(|c| c.get("name").and_then(Value::as_str))
    .collect()
}

fn phase(pod: &DynamicObject) -> &str {
    pod.data
        .get("status")
        .and_then(|s| s.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The first container in `pod` that mounts `claim`, with its mount path.
fn mount_in(pod: &DynamicObject, claim: &str) -> Option<Mount> {
    let spec = pod.data.get("spec")?;
    // Volume names are unique within a pod, so the claim resolves to at most
    // one of them.
    let volume = spec
        .get("volumes")?
        .as_array()?
        .iter()
        .find(|v| {
            v.get("persistentVolumeClaim")
                .and_then(|p| p.get("claimName"))
                .and_then(Value::as_str)
                == Some(claim)
        })
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)?;

    let mut best: Option<Mount> = None;
    let running = running_containers(pod);
    // Sidecars (init containers with `restartPolicy: Always`, GA since 1.29)
    // and debug containers run alongside the app and mount the same volumes;
    // skipping them would report a claim as unmounted and offer a helper pod
    // for a volume that is already attached — which, for ReadWriteOnce, would
    // then never schedule.
    //
    // `status.phase == "Running"` is not enough on its own to exec into any
    // one of them: a pod in CrashLoopBackOff is `Running` with nothing to
    // enter, and a *completed* init container is in the spec forever. Picking
    // either would report the claim as reachable and suppress the helper-pod
    // offer, leaving the user in a dead end — so each candidate is checked
    // against its own status.
    let candidates = [
        ("containers", false),
        ("initContainers", true),
        ("ephemeralContainers", false),
    ]
    .into_iter()
    .filter_map(|(key, sidecar_only)| Some((spec.get(key)?.as_array()?, sidecar_only)))
    .flat_map(|(list, sidecar_only)| list.iter().map(move |c| (c, sidecar_only)));
    for (c, sidecar_only) in candidates {
        let Some(name) = c.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !running.contains(name) {
            continue;
        }
        // A plain init container that happens to be running right now is
        // mid-initialisation and about to exit; only a native sidecar stays.
        if sidecar_only && c.get("restartPolicy").and_then(Value::as_str) != Some("Always") {
            continue;
        }
        let mounts = c
            .get("volumeMounts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for m in mounts {
            if m.get("name").and_then(Value::as_str) != Some(volume) {
                continue;
            }
            let Some(path) = m.get("mountPath").and_then(Value::as_str) else {
                continue;
            };
            // A subPath mount shows only part of the volume, but it is still
            // the only view that container has of it — browsing it is correct,
            // and the alternative is refusing to browse at all.
            let mount = Mount {
                pod: pod.metadata.name.clone().unwrap_or_default(),
                container: name.to_string(),
                path: path.to_string(),
                read_only: m.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
                helper: false,
            };
            if !mount.read_only {
                return Some(mount);
            }
            best.get_or_insert(mount);
        }
    }
    best
}

/// The helper pod sofka creates when nothing mounts the claim: one sleeping
/// container with the volume at [`HELPER_MOUNT`], `generateName` so two
/// sessions never collide, and two independent expiries — the shell's `sleep`
/// and `activeDeadlineSeconds` — so the pod goes away even if sofka is killed
/// before it can delete it.
pub fn helper_pod(claim: &str, image: &str, ttl_secs: u64) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "generateName": HELPER_PREFIX,
            "labels": {
                HELPER_LABELS[0].0: HELPER_LABELS[0].1,
                HELPER_LABELS[1].0: HELPER_LABELS[1].1,
            },
            // A label value can't hold every legal claim name (63 chars, and
            // claims may be longer), so the claim goes in an annotation.
            "annotations": { HELPER_ANNOTATION: claim },
        },
        "spec": {
            "restartPolicy": "Never",
            "activeDeadlineSeconds": ttl_secs,
            "terminationGracePeriodSeconds": 0,
            "automountServiceAccountToken": false,
            "securityContext": { "seccompProfile": { "type": "RuntimeDefault" } },
            "containers": [{
                "name": "explore",
                "image": image,
                "command": ["sh", "-c", format!("sleep {ttl_secs}")],
                "volumeMounts": [{ "name": "pvc", "mountPath": HELPER_MOUNT }],
                "resources": { "requests": { "cpu": "10m", "memory": "16Mi" } },
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "capabilities": { "drop": ["ALL"] },
                },
            }],
            "volumes": [{
                "name": "pvc",
                "persistentVolumeClaim": { "claimName": claim },
            }],
        },
    })
}

/// Parse `ls -A -l` output into entries, sorted directories-first then by name.
///
/// Both GNU coreutils and busybox lay the line out as mode, links, owner,
/// group, size, then three date fields, then the name — so the name is
/// everything past the eighth field, spaces and all. Device nodes replace the
/// single size field with `major, minor`, which shifts the name by one; the
/// mode's leading character says when that happens.
pub fn parse_listing(stdout: &str) -> Vec<Entry> {
    parse_body(stdout).0
}

/// [`parse_listing`], plus a count of body lines that produced no entry —
/// the signal that separates "this directory is empty" from "this is not the
/// `ls` output we know how to read".
fn parse_body(stdout: &str) -> (Vec<Entry>, usize, usize) {
    let mut out = Vec::new();
    let mut unparsed = 0usize;
    let mut unnameable = 0usize;
    for line in stdout.lines() {
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        // From here, anything that bails out is a line we could not read.
        let Some(kind) = line.chars().next().and_then(entry_kind) else {
            unparsed += 1;
            continue;
        };
        // `ls` could not stat this entry, so it printed placeholders: the mode
        // is all `?` and the owner/size columns collapse to one `?` each with
        // a single `?` for the whole timestamp — six fields, not eight.
        // Dropping the line would hide the file entirely; it exists, we just
        // can't size it.
        let unknown = unstattable(line);
        let fields = match () {
            _ if unknown => 6,
            // Device nodes print "major, minor" where a file prints its size.
            _ if matches!(line.as_bytes()[0], b'b' | b'c') => 9,
            _ => 8,
        };
        let Some((head, rest)) = split_fields(line, fields) else {
            unparsed += 1;
            continue;
        };
        // A row whose name field is empty is a file whose name *begins* with
        // a newline: a real file, just not one that can be named. Counted
        // apart from `unparsed`, so a single such name neither makes a
        // readable directory look unreadable nor lets one holding only such
        // files read as empty.
        if rest.is_empty() {
            unnameable += 1;
            continue;
        }
        let (name, link_target) = match kind {
            EntryKind::Link => match rest.split_once(" -> ") {
                Some((n, t)) => (n, t),
                None => (rest, ""),
            },
            _ => (rest, ""),
        };
        if !plausible_name(name) {
            unparsed += 1;
            continue;
        }
        out.push(Entry {
            name: name.to_string(),
            kind,
            // A directory's `ls` size is its own inode's, not the tree's;
            // reporting it would be worse than reporting nothing.
            size: if kind == EntryKind::Dir || unknown {
                None
            } else {
                head[4].parse().ok()
            },
            link_target: link_target.to_string(),
        });
    }
    sort_entries(&mut out);
    (out, unparsed, unnameable)
}

/// Whether `ls` printed this row's metadata as `?` placeholders. The type
/// character is still real (`ls` gets it from the directory entry), so only
/// the permission bits after it are checked.
fn unstattable(line: &str) -> bool {
    let mode = line.split_whitespace().next().unwrap_or_default();
    mode.len() > 1 && mode[1..].bytes().all(|b| b == b'?')
}

fn entry_kind(mode: char) -> Option<EntryKind> {
    match mode {
        'd' => Some(EntryKind::Dir),
        'l' => Some(EntryKind::Link),
        // A filesystem without `d_type` (NFS, XFS without ftype) makes `ls`
        // print `?` for the type as well when it could not stat the entry.
        // It is still a thing that exists on the volume.
        '-' | 'b' | 'c' | 'p' | 's' | '?' => Some(EntryKind::File),
        _ => None,
    }
}

/// Split the first `n` whitespace-separated fields off `line`, returning them
/// with the untouched remainder — which keeps any run of spaces inside a file
/// name that `split_whitespace` would have eaten.
fn split_fields(line: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut rest = line;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    // Both implementations pad *between* columns but put exactly one space
    // before the name, so only that one is a separator: trimming the run would
    // rename a file called " report.txt" and make one called " " vanish.
    let mut chars = rest.chars();
    if !chars.next()?.is_whitespace() {
        return None;
    }
    Some((fields, chars.as_str()))
}

/// Directories first, then case-insensitive by name — the ordering every file
/// manager uses, and the one that makes a deep tree navigable by eye.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Read one local directory for the left pane. Sizes come from the metadata we
/// already have; symlinks are reported as links without following them, so a
/// dangling one lists instead of erroring.
pub fn read_local(dir: &Path) -> Result<(Vec<Entry>, bool), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    let mut truncated = false;
    for entry in read {
        // Capped like the remote pane, and for the same reason: a spool or
        // cache directory with a million files would otherwise be read and
        // sorted in full, on the UI thread, before the first frame.
        if out.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        // An entry we cannot stat still exists — the remote pane goes out of
        // its way to keep those visible, so this one must not drop them.
        let meta = entry.metadata().ok();
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match meta.as_ref() {
            Some(m) if m.file_type().is_symlink() => EntryKind::Link,
            Some(m) if m.is_dir() => EntryKind::Dir,
            Some(_) => EntryKind::File,
            // `read_dir` gave us the name but not the metadata; treat it as a
            // file of unknown size rather than pretending it isn't there.
            None => EntryKind::File,
        };
        // The remote pane gets link targets from `ls -l` for free; read them
        // here too, so both sides describe a symlink the same way.
        let link_target = match kind {
            EntryKind::Link => std::fs::read_link(entry.path())
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        out.push(Entry {
            name,
            kind,
            size: match (kind, meta) {
                (EntryKind::Dir, _) => None,
                (_, Some(m)) => Some(m.len()),
                (_, None) => None,
            },
            link_target,
        });
    }
    sort_entries(&mut out);
    Ok((out, truncated))
}

/// Append `name` to `base` as a POSIX path. `base` is always absolute here —
/// it starts at a mount path and only ever grows by one component at a time.
pub fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The parent of `path`, or `None` at `root`. The browser is confined to the
/// mount: there is nothing above it worth showing, and the rest of the
/// container's filesystem is not what the user asked to look at.
pub fn parent_path(path: &str, root: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let root_trimmed = root.trim_end_matches('/');
    if trimmed == root_trimmed || trimmed.is_empty() {
        return None;
    }
    let parent = match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    };
    // A mount path deeper than "/" means the root itself has a parent we must
    // not walk into. Compared on a path boundary, not as a bare prefix, or
    // "/pvcx" would pass for a root of "/pvc".
    let inside = parent == root_trimmed
        || parent
            .strip_prefix(root_trimmed)
            .is_some_and(|tail| tail.starts_with('/'));
    if !root_trimmed.is_empty() && !inside {
        return Some(root.to_string());
    }
    Some(parent)
}

/// Bytes as a short human-readable size: at most three significant figures,
/// no space before the unit. Binary units, like `ls -h` — deliberately not the
/// PVC table's CAPACITY cell, which passes the Kubernetes quantity string
/// (`10Gi`) through untouched.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn parses_gnu_and_busybox_long_listings() {
        // First block is GNU coreutils, second is busybox — different column
        // widths, same field order.
        let out = "total 12\n\
             drwxr-xr-x 2 root root 4096 Jan  1 00:00 subdir\n\
             -rw-r--r-- 1 root root  128 Jan  1 00:00 data.json\n\
             lrwxrwxrwx 1 root root    9 Jan  1 00:00 current -> data.json\n\
             -rw-r--r--    1 root     root            12 Jan  1 00:00 busybox.txt\n";
        let entries = parse_listing(out);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["subdir", "busybox.txt", "current", "data.json"]
        );
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].size, Some(12));
        let link = entries.iter().find(|e| e.name == "current").unwrap();
        assert_eq!(link.kind, EntryKind::Link);
        assert_eq!(link.link_target, "data.json");
    }

    #[test]
    fn entries_ls_could_not_stat_still_list() {
        // GNU prints placeholders for an entry it cannot stat (an NFS
        // root_squash mount) and exits non-zero, but the rest of the listing
        // is good — dropping the line would hide the file entirely.
        let out = "total 0\n\
             -????????? ? ? ? ?            ? secret.dat\n\
             -rw-r--r-- 1 root root 12 Jan  1 00:00 readable.txt\n";
        let entries = parse_listing(out);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["readable.txt", "secret.dat"]
        );
        assert_eq!(entries[1].size, None, "an unstattable entry has no size");
    }

    #[test]
    fn a_link_target_containing_an_arrow_splits_at_the_first_one() {
        let entries = parse_listing("lrwxrwxrwx 1 root root 9 Jan  1 00:00 a -> b -> c\n");
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[0].link_target, "b -> c");
    }

    #[test]
    fn every_run_mints_a_fresh_nonce() {
        assert_ne!(list_probe("/srv").nonce, list_probe("/srv").nonce);
    }

    #[test]
    fn the_listing_script_caps_output_and_carries_ls_status() {
        let probe = list_probe("/srv");
        let script = probe.script;
        assert!(
            script.contains(&format!("exit {EXIT_NOT_A_DIRECTORY}")),
            "{script}"
        );
        // The cap is applied in the pod, not after the fact…
        assert!(
            script.contains(&format!("head -n {}", MAX_ENTRIES + 2)),
            "{script}"
        );
        // …which costs the pipeline's status, so `ls`'s own is echoed, keyed
        // by a nonce a file name on the volume cannot predict.
        assert!(
            script.contains(&format!("{STATUS_MARKER}{}:", probe.nonce)),
            "{script}"
        );
        // The path is only ever "$1" — never interpolated into the script.
        assert!(script.contains(r#"cd -- "$1""#), "{script}");
    }

    #[test]
    fn leading_spaces_belong_to_the_file_name() {
        // `ls` pads *between* columns but puts exactly one space before the
        // name, so a name that starts with a space is data, not padding.
        // Trimming the run renamed " report.txt" and made " " vanish while the
        // local pane still showed it.
        let entries = parse_listing(
            "-rw-r--r-- 1 root root 0 Jan  1 00:00  report.txt\n\
             -rw-r--r-- 1 root root 0 Jan  1 00:00   two\n\
             -rw-r--r-- 1 root root 0 Jan  1 00:00  \n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [" ", "  two", " report.txt"]
        );
    }

    #[test]
    fn read_local_sorts_keeps_and_caps_like_the_remote_pane() {
        let dir = std::env::temp_dir().join(format!("sofka-read-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Beta")).unwrap();
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::write(dir.join("z.txt"), b"hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("z.txt", dir.join("link")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();

        let (entries, truncated) = read_local(&dir).expect("readable");
        assert!(!truncated);
        // Directories first, then case-insensitive by name.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(&names[..2], ["alpha", "Beta"]);
        assert!(entries[0].is_dir() && entries[1].is_dir());
        assert_eq!(entries[0].size, None, "a directory reports no size");

        let file = entries.iter().find(|e| e.name == "z.txt").unwrap();
        assert_eq!(file.size, Some(5));

        #[cfg(unix)]
        {
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, EntryKind::Link);
            assert_eq!(link.link_target, "z.txt", "both panes describe links alike");
            // A dangling link still lists — `DirEntry::metadata` is `lstat`
            // on Unix, so it describes the link itself, exactly as `ls -l`
            // does on the other side rather than following it to nowhere.
            let dangling = entries.iter().find(|e| e.name == "dangling").unwrap();
            assert_eq!(dangling.kind, EntryKind::Link);
            assert_eq!(dangling.link_target, "nowhere");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_local_stops_at_the_cap() {
        let dir = std::env::temp_dir().join(format!("sofka-read-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_ENTRIES + 5 {
            std::fs::write(dir.join(format!("f{i:05}")), b"").unwrap();
        }
        let (entries, truncated) = read_local(&dir).expect("readable");
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(truncated, "the local pane must cap like the remote one");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_local_reports_a_directory_it_cannot_open() {
        let missing = std::env::temp_dir().join("sofka-does-not-exist-9e3f");
        assert!(read_local(&missing).is_err());

        // The case that actually happens on a volume: the directory is there,
        // the process just cannot read it. Never an empty listing.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("sofka-noread-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hidden"), b"x").unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Running as root would read it anyway, and the assertion would be
            // about the test environment rather than the code.
            if std::fs::read_dir(&dir).is_err() {
                let err = read_local(&dir).unwrap_err();
                assert!(err.contains(&dir.display().to_string()), "{err}");
            }
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).ok();
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn keeps_spaces_inside_file_names() {
        let entries = parse_listing("-rw-r--r-- 1 root root 5 Jan  1 00:00 two  words.txt\n");
        assert_eq!(entries[0].name, "two  words.txt");
    }

    #[test]
    fn device_nodes_do_not_shift_the_name() {
        // "1, 3" occupies two fields where a regular file has one.
        let entries = parse_listing("crw-rw-rw- 1 root root 1, 3 Jan  1 00:00 null\n");
        assert_eq!(entries[0].name, "null");
        assert_eq!(entries[0].kind, EntryKind::File);
    }

    /// A pod spec plus the container statuses that say it is actually up —
    /// `find_mount` needs both, since `status.phase` alone says nothing about
    /// whether any individual container can be exec'd into.
    fn running_pod(name: &str, claim: &str, containers: Value, statuses: Value) -> DynamicObject {
        pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": claim}}],
                "containers": containers,
            },
            "status": {"phase": "Running", "containerStatuses": statuses},
        }))
    }

    fn mounted(name: &str, path: &str, read_only: bool) -> Value {
        json!([{ "name": name, "volumeMounts": [
            {"name": "vol", "mountPath": path, "readOnly": read_only}]}])
    }

    fn up(name: &str) -> Value {
        json!([{ "name": name, "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}} }])
    }

    #[test]
    fn find_mount_prefers_a_writable_running_consumer() {
        let ro = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "reader", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "data", "mountPath": "/data", "readOnly": true}]}],
            },
            "status": {"phase": "Running", "containerStatuses": up("app")},
        }));
        let rw = running_pod("writer", "shared", mounted("app", "/srv", false), up("app"));
        let found = find_mount(&[ro, rw], "shared").expect("a consumer");
        assert_eq!(found.pod, "writer");
        assert_eq!(found.path, "/srv");
        assert!(!found.read_only);
    }

    #[test]
    fn find_mount_ignores_pods_that_are_not_running() {
        let pending = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "pending", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Pending"},
        }));
        assert!(find_mount(&[pending], "shared").is_none());
    }

    #[test]
    fn find_mount_ignores_a_claim_the_pod_does_not_use() {
        let other = running_pod(
            "other",
            "elsewhere",
            mounted("app", "/srv", false),
            up("app"),
        );
        assert!(find_mount(&[other], "shared").is_none());
    }

    #[test]
    fn a_crashlooping_pod_is_not_a_way_into_the_volume() {
        // The pod is `Running`; its only container is not. Exec would fail
        // with "container not running", and returning it here would suppress
        // the helper-pod offer and leave the user in a dead end.
        let crashing = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "crasher", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]},
        }));
        assert!(find_mount(&[crashing], "shared").is_none());
    }

    #[test]
    fn a_finished_init_container_is_not_a_way_into_the_volume() {
        // `initContainers` stays in the spec forever. The seed container has
        // exited; only the app container is up, and it mounts nothing.
        let seeded = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "seeded", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "initContainers": [{"name": "seed", "volumeMounts": [
                    {"name": "vol", "mountPath": "/seed"}]}],
                "containers": [{"name": "app"}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "initContainerStatuses": [
                           {"name": "seed", "state": {"terminated": {"exitCode": 0}}}]},
        }));
        assert!(find_mount(&[seeded], "shared").is_none());
    }

    #[test]
    fn a_running_sidecar_is_a_way_into_the_volume() {
        // A native sidecar — an init container with `restartPolicy: Always`,
        // GA since 1.29 — runs for the pod's whole life and mounts the volume.
        let sidecar = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "with-sidecar", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "initContainers": [{"name": "log-shipper", "restartPolicy": "Always",
                                    "volumeMounts": [{"name": "vol", "mountPath": "/logs"}]}],
                "containers": [{"name": "app"}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "initContainerStatuses": up("log-shipper")},
        }));
        let found = find_mount(&[sidecar], "shared").expect("the sidecar mounts it");
        assert_eq!(found.container, "log-shipper");
        assert_eq!(found.path, "/logs");
    }

    #[test]
    fn a_running_ephemeral_container_is_a_way_into_the_volume() {
        let debugged = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "debugged", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app"}],
                "ephemeralContainers": [{"name": "debugger", "volumeMounts": [
                    {"name": "vol", "mountPath": "/mnt"}]}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "ephemeralContainerStatuses": up("debugger")},
        }));
        let found = find_mount(&[debugged], "shared").expect("the debugger mounts it");
        assert_eq!(found.container, "debugger");
    }

    #[test]
    fn find_mount_skips_a_pod_on_its_way_out() {
        // Its exec would be torn down with it, and its volume released.
        let dying = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "dying", "namespace": "default",
                         "deletionTimestamp": "2026-01-01T00:00:00Z"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Running", "containerStatuses": up("app")},
        }));
        assert!(find_mount(&[dying], "shared").is_none());
    }

    const NONCE: &str = "0123456789abcdef0";

    /// Build the stdout a complete run would produce.
    fn output(body: &str, status: i32) -> String {
        format!("{body}sofka-ls-status:{NONCE}:{status}\n")
    }

    fn interpret(
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Result<(Listing, Option<String>), String> {
        interpret_listing(NONCE, exit_code, stdout, stderr)
    }

    /// `ls -l` lines for `n` plain files, as GNU emits them into a pipe.
    fn files(n: usize) -> String {
        (0..n)
            .map(|i| format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 f{i}\n"))
            .collect()
    }

    #[test]
    fn an_unreadable_directory_is_an_error_not_an_empty_one() {
        // Real busybox and GNU behaviour for a directory with mode 0111: `cd`
        // succeeds, `ls` writes only "total 0" and fails. Rendering that as
        // "empty" would tell the user their volume has nothing on it.
        let err = interpret(
            Some(0),
            &output("total 0\n", 1),
            "ls: can't open '.': Permission denied\n",
        )
        .expect_err("an unreadable directory must not read as empty");
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn a_partly_unstattable_directory_lists_with_a_warning() {
        let (listing, warn) = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -????????? ? ? ? ?            ? locked\n\
                 -rw-r--r-- 1 root root 12 Jan  1 00:00 fine.txt\n",
                1,
            ),
            "ls: cannot access 'locked': Permission denied\n",
        )
        .expect("entries came back, so this is a partial success");
        assert_eq!(listing.entries.len(), 2);
        assert!(!listing.truncated);
        assert!(warn.is_some_and(|w| w.contains("Permission denied")));
    }

    #[test]
    fn a_command_that_never_ran_is_an_error_even_with_empty_output() {
        // No marker and no truncation: `sh`/`ls` never reported a status —
        // a missing binary, a pod that isn't running, a denied exec.
        let err = interpret(Some(126), "", "sh: ls: not found\n")
            .expect_err("a failed exec must not read as an empty directory");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn a_missing_marker_with_a_full_buffer_means_truncated_not_failed() {
        // `head` closed the pipe before `ls` could print the marker.
        let body = files(MAX_ENTRIES + 2);
        let (listing, warn) = interpret(Some(0), &body, "").expect("a truncated listing");
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert!(warn.is_none());
    }

    #[test]
    fn a_file_name_cannot_forge_the_status_marker() {
        // GNU `ls` prints names raw into a pipe, so a file called
        // "evil\nsofka-ls-status:0" puts a marker-shaped line through — before
        // the real marker, which `head` then cuts. Without the nonce that
        // makes a truncated listing claim to be complete.
        let mut body = String::from("-rw-r--r-- 1 root root 1 Jan  1 00:00 evil\n");
        body.push_str("sofka-ls-status:0\n");
        body.push_str(&files(MAX_ENTRIES + 1));
        let (listing, _) = interpret(Some(0), &body, "").expect("a listing");
        assert!(
            listing.truncated,
            "a forged marker made a truncated listing look complete"
        );
        assert!(listing.status.is_none());
    }

    #[test]
    fn names_containing_newlines_do_not_fake_truncation() {
        // Truncation is measured in lines, not parsed entries: 3000 files
        // whose names contain a newline are 6000 lines and 3000 entries, and
        // `ls` read the directory perfectly.
        let body: String = (0..3_000)
            .map(|i| format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 a{i}\nb{i}\n"))
            .collect();
        let (listing, warn) = interpret(Some(0), &output(&body, 0), "").expect("a clean listing");
        assert!(!listing.truncated, "a complete listing reported truncation");
        assert!(warn.is_none());
    }

    #[test]
    fn an_unreadable_ls_format_is_an_error_not_an_empty_directory() {
        // GNU `ls` honours TIME_STYLE from the container's environment, and
        // `long-iso` prints a two-field date where the parser expects three —
        // shifting the name column so nothing parses. The script unsets it,
        // but any other `ls` (toybox, a future format) does the same, and
        // reporting a full volume as "empty" is the one outcome to avoid.
        let err = interpret(
            Some(0),
            &output(
                "total 8\n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 plain.txt\n\
                 drwxr-xr-x 2 root root 4096 2026-09-06 22:24 sub\n",
                0,
            ),
            "",
        )
        .expect_err("an unparseable listing must not read as empty");
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_truncated_listing_that_parsed_to_nothing_is_still_an_error() {
        // Plenty of output, none of it in a shape we read. "Unreadable" does
        // not become "empty" just because there was a lot of it.
        let body: String = (0..MAX_ENTRIES + 2)
            .map(|i| format!("-rw-r--r-- 1 root root 3 2026-09-06 22:24 f{i}\n"))
            .collect();
        let err = interpret(Some(0), &body, "").unwrap_err();
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_name_beginning_with_a_newline_is_reported_as_such_not_as_garbage() {
        // The head row carries the metadata and no name; the remainder lands
        // on the next line and cannot be parsed. One leftover per unnameable
        // row is the signature of a newline in a name — more than that is a
        // format we do not read, which is a different message.
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 0 Jan  1 00:00 \nfoo\n", 0),
            "",
        )
        .expect("readable, just unnameable");
        assert_eq!(listing.unnameable, 1);
        assert_eq!(listing.unparsed, 1, "the name's remainder");
        assert!(warn.is_some_and(|w| w.contains("newline")), "wrong message");
    }

    #[test]
    fn one_empty_name_row_does_not_excuse_a_garbage_listing() {
        // The negative side of the `unparsed <= unnameable` guard: a format we
        // cannot read stays an error even when one row happens to look like a
        // newline-leading name.
        let err = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -rw-r--r-- 1 root root 0 Jan  1 00:00 \n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 a.txt\n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 b.txt\n",
                0,
            ),
            "",
        )
        .unwrap_err();
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_directory_of_only_unnameable_files_is_not_empty() {
        // Its one file is named "\n": `ls` prints a row with no name and then
        // the tail. Nothing parses, but the directory is neither unreadable
        // nor empty — it holds a file sofka has no way to name.
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 0 Jan  1 00:00 \n\n", 0),
            "",
        )
        .expect("readable, just unnameable");
        assert!(listing.entries.is_empty());
        assert_eq!(listing.unnameable, 1);
        assert!(
            warn.is_some_and(|w| w.contains("newline")),
            "the pane would have said 'empty'"
        );
    }

    #[test]
    fn one_name_containing_a_newline_does_not_make_a_directory_unreadable() {
        // The tail of such a name is a row with no name field of its own.
        // Counting it as unparseable would let a single adversarial file name
        // hide a directory that `ls` read perfectly.
        let (listing, warn) = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -rw-r--r-- 1 root root 0 Jan  1 00:00 \n\
                 tail-of-the-name\n\
                 -rw-r--r-- 1 root root 3 Jan  1 00:00 real.txt\n",
                0,
            ),
            "",
        )
        .expect("the directory is readable");
        assert!(listing.entries.iter().any(|e| e.name == "real.txt"));
        // The readable entry is listed, and the one that cannot be named is
        // reported rather than silently dropped.
        assert!(warn.is_some_and(|w| w.contains("newline")));
    }

    #[test]
    fn a_genuinely_empty_directory_is_still_empty() {
        let (listing, warn) = interpret(Some(0), &output("total 0\n", 0), "").expect("a listing");
        assert!(listing.entries.is_empty());
        assert_eq!(listing.unparsed, 0);
        assert!(warn.is_none());
    }

    #[test]
    fn the_listing_script_pins_the_output_format_and_needs_both_paths() {
        let script = list_probe("/srv").script;
        // GNU `ls` reshapes its columns from the environment; a container
        // exporting TIME_STYLE would otherwise make every entry unparseable.
        // GNU `ls` takes its date format, its quoting and its size units from
        // the environment; a container that exports any of them would make the
        // listing unparseable, unaddressable, or silently misreport sizes.
        let unset = script
            .lines()
            .find(|l| l.starts_with("unset "))
            .expect("the script unsets nothing");
        for var in ["TIME_STYLE", "QUOTING_STYLE", "BLOCK_SIZE", "LS_BLOCK_SIZE"] {
            assert!(unset.contains(var), "{var} is not unset: {unset}");
        }
        // `cd ""` succeeds in dash and busybox ash, so neither argument may
        // be allowed through empty.
        assert!(script.contains(r#"[ -n "$1" ] && [ -n "$2" ]"#), "{script}");
    }

    #[test]
    fn a_complete_listing_reports_no_truncation() {
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 1 Jan  1 00:00 a\n", 0),
            "",
        )
        .expect("a clean listing");
        assert_eq!(listing.entries.len(), 1);
        assert!(!listing.truncated);
        assert!(warn.is_none());
    }

    #[test]
    fn a_path_that_is_not_a_directory_says_so() {
        let err = interpret(Some(EXIT_NOT_A_DIRECTORY), "", "").unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn a_file_name_cannot_inject_a_row_that_escapes_the_mount() {
        // Exactly what GNU `ls` writes into a pipe for a directory holding a
        // file called "x\ndrwxr-xr-x 2 root root 4096 Jan  1 00:00 ..": names
        // go through raw, so the second half arrives as its own row.
        let entries = parse_listing(
            "total 4\n\
             -rw-r--r-- 1 root root    0 Jan  1 00:00 x\n\
             drwxr-xr-x 2 root root 4096 Jan  1 00:00 ..\n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["x"],
            "a forged '..' row would walk the browser out of the mount"
        );
    }

    #[test]
    fn a_symlink_target_cannot_inject_an_absolute_path() {
        // A link target is arbitrary bytes, so unlike a file name it can carry
        // "/" — and `Path::join` with an absolute path replaces rather than
        // appends, which on download would write anywhere on the local disk.
        let entries = parse_listing(
            "lrwxrwxrwx 1 root root 51 Jan  1 00:00 evil -> t\n\
             -rw-r--r-- 1 root root  7 Jan  1 00:00 /etc/passwd\n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["evil"]
        );
    }

    #[test]
    fn relative_and_separator_bearing_names_never_survive() {
        for forged in [
            "..",
            ".",
            "a/b",
            "/etc/passwd",
            "../../.ssh/authorized_keys",
        ] {
            let line = format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 {forged}\n");
            assert!(
                parse_listing(&line).is_empty(),
                "{forged:?} survived parsing"
            );
        }
        // …while a name that merely contains a dot is ordinary.
        assert_eq!(
            parse_listing("-rw-r--r-- 1 root root 1 Jan  1 00:00 ..hidden\n")[0].name,
            "..hidden"
        );
    }

    #[test]
    fn the_helper_pod_carries_every_piece_of_evidence_the_sweep_requires() {
        let spec = helper_pod("data", "busybox:1.37", 900);
        for (k, v) in HELPER_LABELS {
            assert_eq!(spec["metadata"]["labels"][k], v);
        }
        assert_eq!(spec["metadata"]["annotations"][HELPER_ANNOTATION], "data");
        assert_eq!(spec["metadata"]["generateName"], HELPER_PREFIX);
        // The selector `:pvc-clean` lists with must name every label, or the
        // extra evidence buys nothing.
        let selector = helper_selector();
        for (k, v) in HELPER_LABELS {
            assert!(selector.contains(&format!("{k}={v}")), "{selector}");
        }
    }

    #[test]
    fn a_link_that_leaves_the_mount_is_refused_by_its_own_exit_code() {
        let err = interpret(Some(EXIT_OUTSIDE_MOUNT), "", "").unwrap_err();
        assert!(err.contains("outside the volume"), "{err}");
    }

    #[test]
    fn the_listing_script_resolves_both_sides_before_comparing_them() {
        let probe = list_probe("/srv");
        // A mount path can itself sit behind a symlink; comparing the raw
        // strings would refuse the mount's own root.
        assert!(
            probe
                .script
                .contains(r#"root=$(cd -- "$2" 2>/dev/null && pwd -P)"#)
        );
        // Trailing slash on both sides: the boundary is explicit, so `/pvcx`
        // is not inside `/pvc` and a root of `/` needs no special case.
        assert!(
            probe
                .script
                .contains(r#"case "$(pwd -P)/" in "${root%/}/"*)"#),
            "{}",
            probe.script
        );
        assert!(probe.script.contains(&format!("exit {EXIT_OUTSIDE_MOUNT}")));
        assert_eq!(probe.root, "/srv");
    }

    #[test]
    fn parent_path_stops_at_the_mount_root() {
        assert_eq!(parent_path("/pvc/a/b", "/pvc").as_deref(), Some("/pvc/a"));
        assert_eq!(parent_path("/pvc/a", "/pvc").as_deref(), Some("/pvc"));
        assert_eq!(parent_path("/pvc", "/pvc"), None);
        assert_eq!(parent_path("/pvc/", "/pvc"), None);
        // A one-component root: the parent of its child is the root, not "/".
        assert_eq!(parent_path("/data/x", "/data").as_deref(), Some("/data"));
    }

    #[test]
    fn join_path_does_not_double_the_separator() {
        assert_eq!(join_path("/pvc", "a"), "/pvc/a");
        assert_eq!(join_path("/", "a"), "/a");
    }

    #[test]
    fn human_size_keeps_three_significant_figures() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024 * 20), "20M");
    }

    #[test]
    fn helper_pod_mounts_the_claim_and_expires_twice() {
        let spec = helper_pod("data", "busybox:1.37", 900);
        assert_eq!(spec["spec"]["activeDeadlineSeconds"], 900);
        assert_eq!(spec["spec"]["containers"][0]["command"][2], "sleep 900");
        assert_eq!(
            spec["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"],
            "data"
        );
        assert_eq!(
            spec["spec"]["containers"][0]["volumeMounts"][0]["mountPath"],
            HELPER_MOUNT
        );
        assert_eq!(spec["metadata"]["generateName"], HELPER_PREFIX);
    }
}
