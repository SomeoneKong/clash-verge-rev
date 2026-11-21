use super::{DownloadEvent, SilentUpdater};
use crate::{
    config::Config,
    core::fork_updater::{self, automatic},
};
use anyhow::Result;
use clash_verge_logging::{Type, logging};
use std::{path::PathBuf, time::Duration};
use tauri_plugin_updater::Update;
use tokio_util::sync::CancellationToken;

pub(in crate::core) async fn with_legacy_cache_cleanup<T>(
    component_available: bool,
    cancel: &CancellationToken,
    cache_dir: impl FnOnce() -> Result<PathBuf>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    if component_available && !cancel.is_cancelled() {
        SilentUpdater::delete_cache_in(cache_dir());
    }
    work.await
}

pub(super) async fn while_requested<T>(cancel: &CancellationToken, work: impl Future<Output = T>) -> Option<T> {
    cancel
        .run_until_cancelled(work)
        .await
        .filter(|_| !cancel.is_cancelled())
}

pub(super) async fn confirmed_cache<T, F>(
    mut verify: impl FnMut() -> F,
    confirm: impl Future<Output = bool>,
    cancel: &CancellationToken,
) -> Option<T>
where
    F: Future<Output = Option<T>>,
{
    drop(verify().await?);
    while_requested(cancel, async {
        if !confirm.await {
            return None;
        }
        verify().await
    })
    .await
    .flatten()
}

pub(super) async fn settle_before_install(
    settle: impl Future<Output = ()>,
    startup_probe: Option<impl Future<Output = bool>>,
    cache_dir: impl FnOnce() -> Result<PathBuf>,
) -> bool {
    settle.await;
    if let Some(probe) = startup_probe
        && probe.await
    {
        SilentUpdater::delete_cache_in(cache_dir());
        return false;
    }
    true
}

pub(super) fn install_startup_cache(
    settings: &crate::config::IVerge,
    cancel: &CancellationToken,
    skip_test: impl FnOnce() -> Result<bool>,
    install: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    if !settings.auto_check_update.unwrap_or(true)
        || automatic::enabled_for(settings)
        || skip_test()?
        || cancel.is_cancelled()
    {
        return Ok(false);
    }
    install()?;
    Ok(true)
}

pub(super) fn install_with_window(
    show: impl FnOnce() -> fork_updater::WindowAfterUpdate,
    install: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let show = show();
    install()?;
    show.keep();
    Ok(())
}

impl SilentUpdater {
    fn delete_cache_in(cache_dir: Result<PathBuf>) {
        if let Ok(cache_dir) = cache_dir
            && cache_dir.exists()
        {
            if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
                logging!(
                    warn,
                    Type::System,
                    "Failed to delete update cache {}: {e}",
                    cache_dir.display()
                );
            } else {
                logging!(info, Type::System, "Update cache deleted");
            }
        }
    }

    /// The gate goes with the installation, which may outlast a caller that stops waiting.
    pub(super) async fn install(
        update: Update,
        bytes: Vec<u8>,
        gate: tokio::sync::MutexGuard<'static, ()>,
        startup: Option<CancellationToken>,
    ) -> Result<bool> {
        // On Windows the installer ends the app right away, with its core left running.
        if !settle_before_install(
            crate::feat::settle_config_updates(),
            startup.as_ref().map(|_| fork_updater::available()),
            Self::cache_dir,
        )
        .await
        {
            return Ok(false);
        }
        anyhow::ensure!(
            !fork_updater::update_in_progress(),
            "cannot confirm that the installation is idle"
        );
        let settings = Config::verge().await;
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let install = || install_with_window(fork_updater::WindowAfterUpdate::show, || Ok(update.install(&bytes)?));
            if let Some(cancel) = startup {
                return install_startup_cache(
                    &settings.latest_arc(),
                    &cancel,
                    || automatic::skip_test(&update),
                    install,
                );
            }
            install()?;
            Ok(true)
        })
        .await?
    }

    pub(super) async fn confirmed_startup_cache(
        app: &tauri::AppHandle,
        version: &str,
        cancel: &CancellationToken,
    ) -> Option<(Update, Vec<u8>)> {
        confirmed_cache(
            || Self::startup_cache(app, version, cancel),
            Self::ask_user_to_install(app, version),
            cancel,
        )
        .await
    }

    async fn startup_cache(
        app: &tauri::AppHandle,
        version: &str,
        cancel: &CancellationToken,
    ) -> Option<(Update, Vec<u8>)> {
        if cancel.is_cancelled() || automatic::enabled().await {
            return None;
        }
        let update = match Self::check(app, cancel).await {
            Ok(Some(u)) if u.version == version => u,
            Ok(_) => {
                Self::delete_cache();
                return None;
            }
            Err(e) => {
                logging!(warn, Type::System, "Checking the cached update: {e:#}");
                return None;
            }
        };
        if automatic::skip_test(&update).ok()? {
            Self::delete_cache();
            return None;
        }
        let bytes = Self::verified_cache(app, &update)?;
        if cancel.is_cancelled()
            || automatic::enabled().await
            || !Config::verge().await.latest_arc().auto_check_update.unwrap_or(true)
            || automatic::skip_test(&update).ok()?
        {
            return None;
        }
        Some((update, bytes))
    }

    pub(in crate::core) fn background_token(&self) -> CancellationToken {
        self.background_cancel
            .lock()
            .get_or_insert_with(CancellationToken::new)
            .clone()
    }

    pub(crate) fn update_schedule(patch: &crate::config::IVerge) {
        if patch.auto_check_update.is_some() || patch.auto_install_update.is_some() {
            Self::global().schedule_changed.notify_one();
        }
        if patch.auto_install_update.is_some() && patch.auto_check_update != Some(false) {
            let cancel = Self::global().background_cancel.lock().take();
            if let Some(cancel) = cancel {
                cancel.cancel();
            }
        }
    }

    pub(super) async fn legacy_background_offer(
        app_handle: &tauri::AppHandle,
        cancel: &CancellationToken,
    ) -> Result<Option<(Update, bool)>> {
        let (offered, component_available) = automatic::background_offer(app_handle, cancel, Self::cache_dir).await?;
        let auto_install = automatic::enabled().await;
        if auto_install || cancel.is_cancelled() {
            return Ok(None);
        }
        let Some(update) = offered else {
            if !component_available {
                Self::delete_cache();
            }
            return Ok(None);
        };
        if automatic::skip_test(&update)? {
            Self::delete_cache();
            return Ok(None);
        }

        Ok(Some((update, component_available)))
    }

    pub(super) async fn prepare_fallback(
        &self,
        version: &str,
        reason: &str,
        cancel: &CancellationToken,
        on_event: &(impl Fn(DownloadEvent) + Sync),
    ) -> Result<()> {
        if fork_updater::update_in_progress() {
            self.manual_cancel.lock().take();
            anyhow::bail!("cannot confirm that the installation is idle");
        }
        on_event(DownloadEvent::Fallback {
            reason: reason.to_owned(),
        });
        let report = fork_updater::report_fallback(version, reason);
        let _ = while_requested(cancel, tokio::time::timeout(Duration::from_secs(3), report)).await;
        Ok(())
    }

    pub(super) async fn wait_for_next_check(&self) {
        // Recalculate the next deadline when the user changes either update setting.
        let finished = tokio::time::Instant::now();
        loop {
            let changed = self.schedule_changed.notified();
            let hours = if automatic::enabled().await { 6 } else { 24 };
            tokio::select! {
                () = tokio::time::sleep_until(finished + Duration::from_secs(hours * 3600)) => break,
                () = changed => {}
            }
        }
    }
}
