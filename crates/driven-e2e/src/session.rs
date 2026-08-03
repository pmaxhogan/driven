//! One hermetic app session: a private tauri-driver + the real `driven-app`
//! binary booted against a scenario-owned `DRIVEN_DATA_DIR`.
//!
//! Each [`AppSession`] owns:
//! - a scratch data dir (fresh `state.db` + logs) - the isolation seam added
//!   for this harness (`DRIVEN_DATA_DIR`);
//! - its own `tauri-driver` child on a free port pair, spawned WITH the
//!   scenario's env (fake remote on/off, fault plan, e2e hooks), because the
//!   app inherits the driver's environment - this is how per-scenario env
//!   works without restarting the container;
//! - a `thirtyfour` WebDriver session whose `tauri:options.application`
//!   launches the real binary.
//!
//! Dropping the session tears everything down (WebDriver DELETE session quits
//! the app; the driver child is killed). On failure the caller copies the data
//! dir's `logs/` into the artifacts dir before drop so evidence survives.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use thirtyfour::prelude::*;

/// Where the suite reads its environment from (all set by the e2e image, all
/// overridable for a host-side run against a held container).
pub struct Env;

impl Env {
    /// Path to the app binary tauri-driver launches.
    pub fn app_binary() -> String {
        std::env::var("DRIVEN_E2E_APP_BINARY")
            .unwrap_or_else(|_| "/usr/local/bin/driven-app".to_string())
    }

    /// Path to the tauri-driver binary.
    pub fn tauri_driver() -> String {
        std::env::var("DRIVEN_E2E_TAURI_DRIVER").unwrap_or_else(|_| "tauri-driver".to_string())
    }

    /// Path to the WebKitWebDriver binary the driver proxies to.
    pub fn native_driver() -> String {
        std::env::var("DRIVEN_E2E_NATIVE_DRIVER")
            .unwrap_or_else(|_| "/usr/bin/WebKitWebDriver".to_string())
    }

    /// The artifacts dir (screenshots, page sources, preserved logs).
    pub fn artifacts_dir() -> PathBuf {
        std::env::var("DRIVEN_E2E_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/driven-e2e-artifacts"))
    }
}

/// Per-session configuration (the scenario's environment contract).
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// `DRIVEN_USE_FAKE_REMOTE=1` - the in-memory fake Drive.
    pub fake_remote: bool,
    /// When set, written to `<data_dir>/fault-plan.json` and exported as
    /// `DRIVEN_TEST_FAULT_PLAN` (requires `fake_remote`).
    pub fault_plan_json: Option<String>,
    /// Reuse an existing data dir (restart-the-app scenarios). `None` = fresh.
    pub data_dir: Option<PathBuf>,
    /// Extra env vars for the app process.
    pub extra_env: Vec<(String, String)>,
}

/// A live app session (see module docs).
pub struct AppSession {
    /// The WebDriver handle. Public: scenarios drive the UI through it.
    pub driver: WebDriver,
    /// The scenario-owned data dir (state.db, logs, fault plan).
    pub data_dir: PathBuf,
    /// Keep the tempdir guard alive so the dir survives the session; `None`
    /// when the caller supplied an existing dir.
    _tempdir: Option<tempfile::TempDir>,
    /// The tauri-driver child process.
    driver_child: std::process::Child,
    /// Base URL of the private tauri-driver (`http://127.0.0.1:<port>`).
    pub driver_url: String,
}

impl AppSession {
    /// Boot a fresh app session per `config`.
    pub async fn launch(config: SessionConfig) -> anyhow::Result<Self> {
        let (tempdir, data_dir) = match &config.data_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                (None, dir.clone())
            }
            None => {
                let td = tempfile::Builder::new().prefix("driven-e2e-").tempdir()?;
                let dir = td.path().to_path_buf();
                (Some(td), dir)
            }
        };

        let mut envs: Vec<(String, String)> = vec![
            ("DRIVEN_DATA_DIR".into(), data_dir.display().to_string()),
            ("DRIVEN_E2E_HOOKS".into(), "1".into()),
        ];
        // Explicit in BOTH directions: the value must never leak in from the
        // container/image environment (a stray =1 silently fakes out every
        // real-backend scenario).
        envs.push((
            "DRIVEN_USE_FAKE_REMOTE".into(),
            if config.fake_remote { "1" } else { "0" }.into(),
        ));
        if let Some(plan) = &config.fault_plan_json {
            anyhow::ensure!(
                config.fake_remote,
                "a fault plan requires fake_remote (the plan only exists on the fake store)"
            );
            let plan_path = data_dir.join("fault-plan.json");
            std::fs::write(&plan_path, plan)?;
            envs.push((
                "DRIVEN_TEST_FAULT_PLAN".into(),
                plan_path.display().to_string(),
            ));
        }
        envs.extend(config.extra_env.clone());

        // Two free ports: the WebDriver front and the native-driver back.
        let port = free_port()?;
        let native_port = free_port()?;
        let driver_url = format!("http://127.0.0.1:{port}");

        let mut cmd = std::process::Command::new(Env::tauri_driver());
        cmd.arg("--port")
            .arg(port.to_string())
            .arg("--native-port")
            .arg(native_port.to_string())
            .arg("--native-driver")
            .arg(Env::native_driver())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in &envs {
            cmd.env(k, v);
        }
        let driver_child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", Env::tauri_driver()))?;

        // Wait for the driver to answer /status.
        wait_http_ready(&format!("{driver_url}/status"), Duration::from_secs(15)).await?;

        let mut caps = Capabilities::new();
        caps.set(
            "tauri:options",
            json!({ "application": Env::app_binary() }),
        )
        .context("setting the tauri:options capability")?;
        let driver = WebDriver::new(&driver_url, caps)
            .await
            .context("creating the WebDriver session (app launch)")?;

        let session = Self {
            driver,
            data_dir,
            _tempdir: tempdir,
            driver_child,
            driver_url,
        };
        tracing::info!(
            driver = %session.driver_url,
            data_dir = %session.data_dir.display(),
            "app session booted"
        );
        // The webview needs a beat to boot Vue; wait for the app root to exist.
        session
            .wait_for_js("!!document.querySelector('#app')", Duration::from_secs(20))
            .await
            .context("app root #app never appeared")?;
        Ok(session)
    }

    /// Invoke a Tauri IPC command inside the app (the production
    /// `window.__TAURI_INTERNALS__.invoke` path) and return its JSON result.
    /// A rejected invoke becomes `Err` carrying the serialized command error.
    pub async fn invoke(&self, cmd: &str, args: Value) -> anyhow::Result<Value> {
        let script = r#"
            const done = arguments[arguments.length - 1];
            const [cmd, args] = arguments;
            window.__TAURI_INTERNALS__.invoke(cmd, args).then(
                (ok) => done({ ok: ok === undefined ? null : ok }),
                (err) => done({ err: (err && typeof err === 'object') ? err : String(err) }),
            );
        "#;
        let ret = self
            .driver
            .execute_async(script, vec![json!(cmd), args])
            .await
            .with_context(|| format!("execute_async invoke({cmd})"))?;
        let v: Value = ret.json().clone();
        if let Some(err) = v.get("err") {
            anyhow::bail!("invoke({cmd}) rejected: {err}");
        }
        Ok(v.get("ok").cloned().unwrap_or(Value::Null))
    }

    /// Evaluate a JS expression and return its JSON value.
    pub async fn eval(&self, expr: &str) -> anyhow::Result<Value> {
        let ret = self
            .driver
            .execute(&format!("return ({expr});"), vec![])
            .await
            .with_context(|| format!("eval: {expr}"))?;
        Ok(ret.json().clone())
    }

    /// Poll a boolean JS expression until it is true or `timeout` elapses.
    pub async fn wait_for_js(&self, expr: &str, timeout: Duration) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        loop {
            match self.eval(expr).await {
                Ok(Value::Bool(true)) => return Ok(()),
                Ok(_) => {}
                // Early boot: the webview may not be ready to execute yet.
                Err(_) if start.elapsed() < Duration::from_secs(5) => {}
                Err(e) => return Err(e),
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for js condition: {expr}");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Wait until a CSS selector matches a visible element.
    pub async fn wait_for_selector(&self, css: &str, timeout: Duration) -> anyhow::Result<()> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({css:?}); \
             return !!el && el.offsetParent !== null; }})()"
        );
        self.wait_for_js(&expr, timeout)
            .await
            .with_context(|| format!("selector never visible: {css}"))
    }

    /// Click the element matching `css` (via the DOM, which works regardless
    /// of native window focus).
    pub async fn click(&self, css: &str) -> anyhow::Result<()> {
        let clicked = self
            .eval(&format!(
                "(() => {{ const el = document.querySelector({css:?}); \
                 if (!el) return false; el.click(); return true; }})()"
            ))
            .await?;
        anyhow::ensure!(clicked == Value::Bool(true), "no element to click: {css}");
        Ok(())
    }

    /// Navigate the SPA to `path` via the app's own client-side router link
    /// handling (history push + a popstate so vue-router notices).
    pub async fn goto(&self, path: &str) -> anyhow::Result<()> {
        self.driver
            .execute(
                &format!(
                    "history.pushState(null, '', {path:?}); \
                     window.dispatchEvent(new PopStateEvent('popstate'));"
                ),
                vec![],
            )
            .await
            .with_context(|| format!("goto {path}"))?;
        Ok(())
    }

    /// Save a PNG screenshot of the webview into `dir/<name>.png`.
    pub async fn screenshot(&self, dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{name}.png"));
        let png = self.driver.screenshot_as_png().await?;
        std::fs::write(&path, png)?;
        Ok(path)
    }

    /// Preserve the session's evidence (app logs + page source) into `dir`.
    /// Called by the runner on scenario failure BEFORE the session drops.
    pub async fn preserve_evidence(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(src) = self.driver.source().await {
            let _ = std::fs::write(dir.join("page-source.html"), src);
        }
        let logs = self.data_dir.join("logs");
        if logs.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&logs) {
                for e in entries.flatten() {
                    let _ = std::fs::copy(e.path(), dir.join(e.file_name()));
                }
            }
        }
    }

    /// Quit the app (delete the WebDriver session) but keep the data dir for a
    /// follow-up session (settings-persistence restart scenarios). Consumes
    /// self; the tempdir guard is returned so the dir outlives the session.
    pub async fn quit_keep_data_dir(
        mut self,
    ) -> anyhow::Result<(PathBuf, Option<tempfile::TempDir>)> {
        let _ = self.driver.clone().quit().await;
        let _ = self.driver_child.kill();
        let _ = self.driver_child.wait();
        Ok((self.data_dir.clone(), self._tempdir.take()))
    }
}

impl Drop for AppSession {
    fn drop(&mut self) {
        // Best-effort teardown. Killing tauri-driver does NOT reliably reap
        // the launched app (observed in-container: the app outlived the
        // driver, and before the single-instance gate landed that stranded
        // app made every later session's launch abort). Scenarios run
        // sequentially, so a broad pkill of the app binary is safe here and
        // guarantees the next session starts clean.
        let _ = self.driver_child.kill();
        let _ = self.driver_child.wait();
        let _ = std::process::Command::new("pkill")
            .args(["-f", "driven-app"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Bind-then-drop to find a free localhost port.
fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Poll a URL until it answers HTTP 200.
pub async fn wait_http_ready(url: &str, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let start = std::time::Instant::now();
    loop {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for {url}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
