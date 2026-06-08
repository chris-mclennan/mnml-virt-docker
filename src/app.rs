//! App state — per-tab list of docker resources + selection cursor.
//! `docker inspect` for the focused row is loaded lazily, mirroring
//! the AWS-family lazy-attributes pattern. Daemon-not-running is a
//! top-level state, not a per-tab error.

use crate::config::{Config, Tab};
use crate::docker::{self, Container, DaemonState, Image, Network, Volume};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct TabSpec {
    pub kind: TabKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabKind {
    Containers,
    Images,
    Volumes,
    Networks,
    /// Compose tab — `compose_file` is the resolved absolute path to
    /// `docker-compose.yml` inside the project directory.
    Compose {
        compose_file: PathBuf,
    },
}

impl TabKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TabKind::Containers => "containers",
            TabKind::Images => "images",
            TabKind::Volumes => "volumes",
            TabKind::Networks => "networks",
            TabKind::Compose { .. } => "compose",
        }
    }
}

impl TabSpec {
    pub fn resolve(t: &Tab) -> Result<Self> {
        let kind = match t.kind.as_str() {
            "containers" => TabKind::Containers,
            "images" => TabKind::Images,
            "volumes" => TabKind::Volumes,
            "networks" => TabKind::Networks,
            "compose" => {
                let dir = t.project_path.as_deref().unwrap_or("").trim();
                if dir.is_empty() {
                    anyhow::bail!("tab `{}`: kind=\"compose\" requires `project_path`", t.name);
                }
                let mut p = PathBuf::from(dir);
                // If the user pointed at a file directly, use it as-is.
                // Otherwise (directory or nonexistent path) probe for
                // the conventional compose-file names and fall back
                // to <dir>/docker-compose.yml.
                let looks_like_compose_file = p.extension().is_some()
                    && p.file_name()
                        .map(|n| {
                            let s = n.to_string_lossy();
                            s == "compose.yaml"
                                || s == "compose.yml"
                                || s == "docker-compose.yml"
                                || s == "docker-compose.yaml"
                        })
                        .unwrap_or(false);
                if !looks_like_compose_file {
                    // Prefer compose.yaml (Compose Spec) then compose.yml then docker-compose.yml.
                    let candidates = ["compose.yaml", "compose.yml", "docker-compose.yml"];
                    let mut found = None;
                    for c in candidates {
                        let cand = p.join(c);
                        if cand.exists() {
                            found = Some(cand);
                            break;
                        }
                    }
                    p = found.unwrap_or_else(|| p.join("docker-compose.yml"));
                }
                TabKind::Compose { compose_file: p }
            }
            other => anyhow::bail!("tab `{}`: unknown kind {other:?}", t.name),
        };
        Ok(Self { kind })
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Container(Container),
    Image(Image),
    Volume(Volume),
    Network(Network),
    ComposeService(docker::ComposeService),
}

impl Item {
    /// `(name, secondary)` — left column. The secondary is what
    /// trails the bolded primary in the list row.
    pub fn primary_label(&self) -> String {
        match self {
            Item::Container(c) => c.names.clone(),
            Item::Image(i) => i.repo_tag(),
            Item::Volume(v) => v.name.clone(),
            Item::Network(n) => n.name.clone(),
            Item::ComposeService(s) => s.service.clone(),
        }
    }

    pub fn secondary_label(&self) -> String {
        match self {
            Item::Container(c) => {
                let ports = if c.ports.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", c.ports)
                };
                format!("{}  {}{}", c.short_id(), c.image, ports)
            }
            Item::Image(i) => format!("{}  {}  {}", i.short_id(), i.size, i.created_since),
            Item::Volume(v) => format!("{}  {}", v.driver, v.mountpoint),
            Item::Network(n) => format!("{}  {}  {}", n.short_id(), n.driver, n.scope),
            Item::ComposeService(s) => {
                if s.image.is_empty() {
                    s.status.clone()
                } else {
                    format!("{}  {}", s.status, s.image)
                }
            }
        }
    }

    /// State string used for colour cues — same word used by docker
    /// for containers, plus a synthetic value for the other kinds.
    pub fn state(&self) -> &str {
        match self {
            Item::Container(c) => c.state.as_str(),
            Item::Image(_) => "image",
            Item::Volume(_) => "volume",
            Item::Network(_) => "network",
            Item::ComposeService(s) => s.state.as_str(),
        }
    }

    /// The identifier used for `docker inspect`, action commands,
    /// and clipboard yanks.
    pub fn id(&self) -> &str {
        match self {
            Item::Container(c) => &c.id,
            Item::Image(i) => &i.id,
            Item::Volume(v) => &v.name,
            Item::Network(n) => &n.name,
            Item::ComposeService(s) => &s.name,
        }
    }
}

pub struct ItemsTab {
    pub items: Vec<Item>,
    pub selected: usize,
    pub last_loaded: Option<Instant>,
    pub last_error: Option<String>,
    pub loading: bool,
    /// Pretty-printed `docker inspect` output for the focused item,
    /// lazily fetched.
    pub focused_detail: Option<String>,
    /// Track the id we have detail for, so cursor moves trigger a
    /// refetch.
    pub focused_detail_for: Option<String>,
}

impl ItemsTab {
    fn empty() -> Self {
        ItemsTab {
            items: Vec::new(),
            selected: 0,
            last_loaded: None,
            last_error: None,
            loading: false,
            focused_detail: None,
            focused_detail_for: None,
        }
    }
}

pub struct TabState {
    pub name: String,
    pub spec: TabSpec,
    pub data: ItemsTab,
}

/// Pending destructive action — `R` shows the confirm overlay; `y`
/// commits, `n` / Esc cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RmPending {
    Container(String),
    Image(String),
    Volume(String),
    Network(String),
}

impl RmPending {
    pub fn description(&self) -> String {
        match self {
            RmPending::Container(id) => format!("remove container {}", short(id)),
            RmPending::Image(id) => format!("remove image {}", short(id)),
            RmPending::Volume(name) => format!("remove volume {name}"),
            RmPending::Network(name) => format!("remove network {name}"),
        }
    }
}

fn short(id: &str) -> &str {
    let cap = id.len().min(12);
    &id[..cap]
}

pub struct App {
    pub cfg: Config,
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub status: String,
    pub daemon: DaemonState,
    /// `R` confirmation overlay state — `None` = no pending action.
    pub rm_pending: Option<RmPending>,
    /// Queue of commands the UI loop should spawn as pty / external
    /// processes (logs, exec, sibling launches). The crossterm `ui`
    /// loop spawns them directly via `std::process::Command::spawn`.
    /// The blit loop drops them on the floor today (v0.1 standalone
    /// only — blit is wired through but the host-side pty hand-off
    /// is a v0.2 follow-up).
    pub pending_spawns: Vec<Vec<String>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let mut tabs = Vec::with_capacity(cfg.tabs.len());
        for t in &cfg.tabs {
            let spec = TabSpec::resolve(t)?;
            tabs.push(TabState {
                name: t.name.clone(),
                data: ItemsTab::empty(),
                spec,
            });
        }
        let daemon = docker::probe_daemon();
        let status = match &daemon {
            DaemonState::Ok(v) => format!("daemon: ok · docker server {v}"),
            DaemonState::Offline => {
                "docker daemon not running — start Docker Desktop, then press r".into()
            }
            DaemonState::CliMissing(e) => format!("docker CLI not found: {e}"),
            DaemonState::Error(e) => format!("docker error: {e}"),
        };
        let mut app = App {
            cfg,
            tabs,
            active_tab: 0,
            status,
            daemon,
            rm_pending: None,
            pending_spawns: Vec::new(),
        };
        if matches!(app.daemon, DaemonState::Ok(_)) {
            app.refresh_active();
            app.ensure_focused_loaded();
        }
        Ok(app)
    }

    pub fn active(&self) -> &TabState {
        &self.tabs[self.active_tab]
    }
    pub fn active_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }

    pub fn daemon_online(&self) -> bool {
        matches!(self.daemon, DaemonState::Ok(_))
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            if !self.daemon_online() {
                return;
            }
            if self.tabs[idx].data.items.is_empty() && self.tabs[idx].data.last_error.is_none() {
                self.refresh_active();
            }
            self.ensure_focused_loaded();
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        {
            let tab = self.active_mut();
            if tab.data.items.is_empty() {
                return;
            }
            let n = tab.data.items.len() as isize;
            let cur = tab.data.selected as isize;
            let next = (cur + delta).clamp(0, n - 1);
            tab.data.selected = next as usize;
        }
        self.ensure_focused_loaded();
    }

    pub fn refresh_active(&mut self) {
        // Re-probe the daemon if we're offline — `r` is the documented
        // "I just started Docker, try again" key.
        if !self.daemon_online() {
            let new_state = docker::probe_daemon();
            self.daemon = new_state;
            match &self.daemon {
                DaemonState::Ok(v) => {
                    self.status = format!("daemon: ok · docker server {v}");
                }
                DaemonState::Offline => {
                    self.status =
                        "docker daemon still offline — start Docker Desktop, then press r".into();
                    return;
                }
                DaemonState::CliMissing(e) => {
                    self.status = format!("docker CLI not found: {e}");
                    return;
                }
                DaemonState::Error(e) => {
                    self.status = format!("docker error: {e}");
                    return;
                }
            }
        }

        let idx = self.active_tab;
        let spec = self.tabs[idx].spec.clone();
        let name = self.tabs[idx].name.clone();
        self.status = format!("loading {name}…");
        self.tabs[idx].data.loading = true;

        let result: Result<Vec<Item>> = match &spec.kind {
            TabKind::Containers => {
                docker::list_containers().map(|cs| cs.into_iter().map(Item::Container).collect())
            }
            TabKind::Images => {
                docker::list_images().map(|is| is.into_iter().map(Item::Image).collect())
            }
            TabKind::Volumes => {
                docker::list_volumes().map(|vs| vs.into_iter().map(Item::Volume).collect())
            }
            TabKind::Networks => {
                docker::list_networks().map(|ns| ns.into_iter().map(Item::Network).collect())
            }
            TabKind::Compose { compose_file } => {
                docker::list_compose_services(compose_file.to_string_lossy().as_ref())
                    .map(|ss| ss.into_iter().map(Item::ComposeService).collect())
            }
        };

        let t = &mut self.tabs[idx];
        t.data.loading = false;
        match result {
            Ok(items) => {
                let count = items.len();
                // Reset focused-detail cache if the focused item changed underneath us.
                let prev_id = t
                    .data
                    .items
                    .get(t.data.selected)
                    .map(|i| i.id().to_string());
                t.data.items = items;
                t.data.selected = t.data.selected.min(count.saturating_sub(1));
                let new_id = t
                    .data
                    .items
                    .get(t.data.selected)
                    .map(|i| i.id().to_string());
                if prev_id != new_id {
                    t.data.focused_detail = None;
                    t.data.focused_detail_for = None;
                }
                t.data.last_loaded = Some(Instant::now());
                t.data.last_error = None;
                self.status = format!(
                    "{name}: {count} {kind_label}",
                    kind_label = spec.kind.as_str()
                );
            }
            Err(e) => {
                // Sniff for daemon-down inside an error and promote
                // to top-level offline state.
                let msg = e.to_string();
                if docker::is_daemon_offline(&msg) {
                    self.daemon = DaemonState::Offline;
                    self.status =
                        "docker daemon not running — start Docker Desktop, then press r".into();
                } else {
                    t.data.last_error = Some(msg.clone());
                    self.status = format!("error: {msg}");
                }
            }
        }
    }

    pub fn ensure_focused_loaded(&mut self) {
        if !self.daemon_online() {
            return;
        }
        let idx = self.active_tab;
        let sel = self.tabs[idx].data.selected;
        let Some(item) = self.tabs[idx].data.items.get(sel) else {
            return;
        };
        let id = item.id().to_string();
        if self.tabs[idx].data.focused_detail_for.as_deref() == Some(id.as_str())
            && self.tabs[idx].data.focused_detail.is_some()
        {
            return;
        }
        let result = match item {
            Item::Container(_) | Item::Image(_) | Item::ComposeService(_) => docker::inspect(&id),
            Item::Volume(_) => docker::inspect_volume(&id),
            Item::Network(_) => docker::inspect_network(&id),
        };
        let t = &mut self.tabs[idx];
        match result {
            Ok(detail) => {
                t.data.focused_detail = Some(detail);
                t.data.focused_detail_for = Some(id);
            }
            Err(e) => {
                t.data.focused_detail = Some(format!("(inspect failed: {e})"));
                t.data.focused_detail_for = Some(id);
            }
        }
    }

    pub fn tick(&mut self) -> bool {
        let interval = self.cfg.refresh_interval_secs;
        if interval == 0 || !self.daemon_online() {
            return false;
        }
        let idx = self.active_tab;
        let stale = match self.tabs[idx].data.last_loaded {
            Some(t) => t.elapsed().as_secs() >= interval,
            None => true,
        };
        if stale && !self.tabs[idx].data.loading {
            self.refresh_active();
            true
        } else {
            false
        }
    }

    pub fn focused_item(&self) -> Option<&Item> {
        let t = self.active();
        t.data.items.get(t.data.selected)
    }

    /// `o` — open Docker Desktop (macOS) or noop with toast (Linux/Win).
    pub fn open_docker_desktop(&mut self) {
        if cfg!(target_os = "macos") {
            match std::process::Command::new("open")
                .args(["-a", "Docker Desktop"])
                .spawn()
            {
                Ok(_) => self.status = "opened Docker Desktop".into(),
                Err(e) => self.status = format!("open Docker Desktop failed: {e}"),
            }
        } else {
            self.status = "Docker Desktop launch only supported on macOS".into();
        }
    }

    /// `y` — yank the focused item's full ID/name.
    pub fn yank_id(&mut self) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let id = item.id().to_string();
        if id.is_empty() {
            self.status = "no ID to yank".into();
            return;
        }
        match crate::clipboard::copy(&id) {
            Ok(()) => self.status = format!("copied: {id}"),
            Err(e) => self.status = format!("copy failed: {e}"),
        }
    }

    /// `l` — tail logs for the focused container in a follow loop.
    /// Containers only; other tabs get a toast.
    pub fn tail_logs(&mut self) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let Item::Container(c) = item else {
            self.status = "logs: only available on containers".into();
            return;
        };
        let id = c.id.clone();
        let label = c.names.clone();
        self.pending_spawns.push(vec![
            "docker".into(),
            "logs".into(),
            "-f".into(),
            id.clone(),
        ]);
        self.status = format!("tailing logs for {label}…");
    }

    /// `e` — exec a shell into the focused running container. Tries
    /// `/bin/bash` first then `/bin/sh`. Other tabs / non-running
    /// containers: toast.
    pub fn exec_shell(&mut self) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let Item::Container(c) = item else {
            self.status = "exec: only available on containers".into();
            return;
        };
        if !c.is_running() {
            self.status = format!("exec: {} is not running", c.names);
            return;
        }
        let id = c.id.clone();
        let label = c.names.clone();
        // We can't `try /bin/bash, fall back to /bin/sh` from a
        // single spawn — punt the decision to the shell. `which`
        // chain works in either busybox or coreutils.
        let chain = "if [ -x /bin/bash ]; then exec /bin/bash; else exec /bin/sh; fi";
        self.pending_spawns.push(vec![
            "docker".into(),
            "exec".into(),
            "-it".into(),
            id,
            "/bin/sh".into(),
            "-c".into(),
            chain.into(),
        ]);
        self.status = format!("exec into {label}…");
    }

    /// `s` — stop focused container. `S` — start. No confirmation —
    /// both are reversible.
    pub fn stop_or_start(&mut self, start: bool) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let Item::Container(c) = item else {
            self.status = "stop/start: only available on containers".into();
            return;
        };
        let id = c.id.clone();
        let label = c.names.clone();
        let res = if start {
            docker::start_container(&id)
        } else {
            docker::stop_container(&id)
        };
        match res {
            Ok(()) => {
                self.status = if start {
                    format!("started {label}")
                } else {
                    format!("stopped {label}")
                };
                self.refresh_active();
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    /// `R` — show the rm-confirmation overlay for the focused item.
    /// Idempotent (calling twice keeps the same pending action).
    pub fn enter_rm_confirm(&mut self) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let pending = match item {
            Item::Container(c) => RmPending::Container(c.id.clone()),
            Item::Image(i) => RmPending::Image(i.id.clone()),
            Item::Volume(v) => RmPending::Volume(v.name.clone()),
            Item::Network(n) => RmPending::Network(n.name.clone()),
            Item::ComposeService(_) => {
                self.status =
                    "rm: not supported for compose services — `docker compose down` instead".into();
                return;
            }
        };
        self.status = format!(
            "{} — y to confirm, n / Esc to cancel",
            pending.description()
        );
        self.rm_pending = Some(pending);
    }

    /// Confirm pending rm.
    pub fn confirm_rm(&mut self) {
        let Some(pending) = self.rm_pending.take() else {
            return;
        };
        let res = match &pending {
            RmPending::Container(id) => docker::rm_container(id),
            RmPending::Image(id) => docker::rmi_image(id),
            RmPending::Volume(name) => docker::rm_volume(name),
            RmPending::Network(name) => docker::rm_network(name),
        };
        match res {
            Ok(()) => {
                self.status = format!("done: {}", pending.description());
                self.refresh_active();
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    /// Cancel pending rm (Esc or `n`).
    pub fn cancel_rm(&mut self) {
        if self.rm_pending.take().is_some() {
            self.status = "rm cancelled".into();
        }
    }

    /// `L` — cross-sibling jump: if focused image is an ECR URL,
    /// spawn `mnml-aws-ecr --region <region>`. Otherwise toast.
    /// Only available on the images tab.
    pub fn handoff_ecr(&mut self) {
        let Some(item) = self.focused_item() else {
            self.status = "no item under cursor".into();
            return;
        };
        let image = match item {
            Item::Image(i) => i.repo_tag(),
            Item::Container(c) => c.image.clone(),
            _ => {
                self.status = "L jump: only available on images / containers".into();
                return;
            }
        };
        let Some((_acct, region)) = docker::parse_ecr_url(&image) else {
            self.status = format!("not an ECR image: {image}");
            return;
        };
        match std::process::Command::new("mnml-aws-ecr")
            .args(["--region", &region])
            .spawn()
        {
            Ok(_) => self.status = format!("launched mnml-aws-ecr ({region})"),
            Err(e) => self.status = format!("spawn mnml-aws-ecr failed (install it?): {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tab;
    use crate::docker::{Container, Image};

    fn cfg_with(tabs: Vec<Tab>) -> Config {
        Config {
            refresh_interval_secs: 0, // disable tick auto-refresh
            tabs,
        }
    }

    #[test]
    fn tab_spec_resolve_known_kinds() {
        for kind in ["containers", "images", "volumes", "networks"] {
            let t = Tab {
                name: kind.into(),
                kind: kind.into(),
                project_path: None,
            };
            let spec = TabSpec::resolve(&t).unwrap();
            assert_eq!(spec.kind.as_str(), kind);
        }
    }

    #[test]
    fn tab_spec_compose_resolves_compose_file() {
        let t = Tab {
            name: "myapp".into(),
            kind: "compose".into(),
            project_path: Some("/nonexistent/dir".into()),
        };
        let spec = TabSpec::resolve(&t).unwrap();
        if let TabKind::Compose { compose_file } = spec.kind {
            // Falls back to <dir>/docker-compose.yml when the dir
            // doesn't exist (we don't probe it then).
            assert!(
                compose_file
                    .to_string_lossy()
                    .ends_with("docker-compose.yml")
            );
        } else {
            panic!("expected compose kind");
        }
    }

    #[test]
    fn rm_state_machine_pending_then_cancel() {
        let cfg = cfg_with(vec![Tab {
            name: "containers".into(),
            kind: "containers".into(),
            project_path: None,
        }]);
        // Construct an App without going through ::new (which would
        // probe the docker daemon) — drop into raw state.
        let mut app = App {
            cfg,
            tabs: vec![TabState {
                name: "containers".into(),
                spec: TabSpec {
                    kind: TabKind::Containers,
                },
                data: ItemsTab::empty(),
            }],
            active_tab: 0,
            status: String::new(),
            daemon: DaemonState::Ok("test".into()),
            rm_pending: None,
            pending_spawns: Vec::new(),
        };
        app.tabs[0].data.items.push(Item::Container(Container {
            id: "abc123def456".into(),
            image: "redis:7".into(),
            names: "redis".into(),
            status: "Up".into(),
            state: "running".into(),
            ports: String::new(),
            running_for: String::new(),
            command: String::new(),
            created_at: String::new(),
        }));
        assert!(app.rm_pending.is_none());
        app.enter_rm_confirm();
        assert_eq!(
            app.rm_pending,
            Some(RmPending::Container("abc123def456".into()))
        );
        app.cancel_rm();
        assert!(app.rm_pending.is_none());
        assert!(app.status.contains("cancelled"));
    }

    #[test]
    fn rm_state_machine_compose_service_rejected() {
        let cfg = cfg_with(vec![Tab {
            name: "compose".into(),
            kind: "containers".into(),
            project_path: None,
        }]);
        let mut app = App {
            cfg,
            tabs: vec![TabState {
                name: "compose".into(),
                spec: TabSpec {
                    kind: TabKind::Containers,
                },
                data: ItemsTab::empty(),
            }],
            active_tab: 0,
            status: String::new(),
            daemon: DaemonState::Ok("test".into()),
            rm_pending: None,
            pending_spawns: Vec::new(),
        };
        app.tabs[0]
            .data
            .items
            .push(Item::ComposeService(docker::ComposeService {
                name: "web-1".into(),
                service: "web".into(),
                state: "running".into(),
                status: "Up".into(),
                image: "redis:7".into(),
                project: "myapp".into(),
            }));
        app.enter_rm_confirm();
        assert!(app.rm_pending.is_none());
        assert!(app.status.contains("compose"));
    }

    #[test]
    fn item_primary_and_secondary_labels() {
        let i = Item::Image(Image {
            id: "sha256:abcdef1234567890".into(),
            repository: "redis".into(),
            tag: "7".into(),
            size: "110MB".into(),
            created_since: "3 weeks ago".into(),
            created_at: String::new(),
            digest: String::new(),
        });
        assert_eq!(i.primary_label(), "redis:7");
        assert!(i.secondary_label().contains("110MB"));
    }
}
