use super::legacy::{
    confirmed_cache, install_startup_cache, install_with_window, settle_before_install, while_requested,
    with_legacy_cache_cleanup,
};
use super::*;
use crate::core::fork_updater::automatic::download_with_sources;
use std::cell::Cell;
use std::future::ready;

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn a_recovered_component_discards_the_legacy_cache_for_a_live_background_check() {
    let dir = std::env::temp_dir().join(format!("verge-legacy-cache-{}", nanoid::nanoid!()));
    let bin = dir.join("pending_update.bin");
    let meta = dir.join("pending_update.json");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(&bin, b"payload").unwrap();
    std::fs::write(&meta, b"{}").unwrap();
    for (online, cancelled) in [(false, false), (true, true), (true, false)] {
        let cancel = CancellationToken::new();
        if cancelled {
            cancel.cancel();
        }
        let offered =
            with_legacy_cache_cleanup(online, &cancel, || Ok(dir.clone()), ready(Ok(Some("ordinary release"))))
                .await
                .unwrap();
        assert_eq!(offered, Some("ordinary release"));
        assert_eq!(bin.exists(), !online || cancelled);
        assert_eq!(meta.exists(), !online || cancelled);
    }
}

#[tokio::test(start_paused = true)]
#[allow(clippy::unwrap_used)]
async fn a_stalled_source_switches_and_a_progressing_download_can_exceed_five_minutes() {
    let origin = tauri::Url::parse("https://github.com/example/update/releases/download/v2/payload.zip").unwrap();
    let start = tokio::time::Instant::now();
    let attempts = Cell::new(0);
    let bytes = download_with_sources("2", &origin, |url, received| {
        let n = attempts.get();
        attempts.set(n + 1);
        async move {
            if n == 0 {
                return std::future::pending().await;
            }
            assert!(url.as_str().starts_with(MIRRORS[0]));
            for _ in 0..64 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                received.fetch_add(MIN_PROGRESS, Ordering::Relaxed);
            }
            Ok(vec![42])
        }
    })
    .await
    .unwrap();
    assert_eq!(attempts.get(), 2);
    assert_eq!(bytes, [42]);
    assert!(start.elapsed() >= Duration::from_secs(350));
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_a_check_error_and_a_stalled_report() {
    let cancel = CancellationToken::new();
    let result = while_requested(&cancel, async {
        cancel.cancel();
        Err::<(), _>("check cancelled")
    })
    .await;
    assert_eq!(result, None);
    let cancel = CancellationToken::new();
    let (report, ()) = tokio::join!(while_requested(&cancel, std::future::pending::<()>()), async {
        tokio::task::yield_now().await;
        cancel.cancel();
    });
    assert!(report.is_none());
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn a_component_that_recovers_during_config_settlement_discards_the_confirmed_cache() {
    let dir = std::env::temp_dir().join(format!("verge-recovered-component-{}", nanoid::nanoid!()));
    std::fs::create_dir(&dir).unwrap();
    for file in ["pending_update.bin", "pending_update.json"] {
        std::fs::write(dir.join(file), []).unwrap();
    }
    let online = Cell::new(false);
    let cancel = CancellationToken::new();
    let cache = confirmed_cache(|| ready(Some("installer")), ready(true), &cancel).await;
    assert_eq!(cache, Some("installer"));
    assert!(!online.get());
    let mut calls = 0;
    if settle_before_install(
        async {
            tokio::task::yield_now().await;
            online.set(true);
        },
        Some(async { online.get() }),
        || Ok(dir.clone()),
    )
    .await
    {
        install_startup_cache(
            &Default::default(),
            &cancel,
            || Ok(false),
            || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
    }
    assert_eq!(calls, 0);
    assert!(!dir.exists());
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn confirmed_startup_install_marks_the_window_only_when_installation_starts() {
    use super::super::fork_updater::WindowAfterUpdate;
    let marker = std::env::temp_dir().join(format!("verge-update-test-{}", nanoid::nanoid!()));
    let window = marker.with_extension("show");
    for change in [
        "decline",
        "test marker",
        "auto check",
        "auto install",
        "cancel",
        "error",
        "none",
    ] {
        let cancel = CancellationToken::new();
        std::fs::write(&marker, []).unwrap();
        let cached = confirmed_cache(
            || ready(marker.is_file().then_some(())),
            ready(change != "decline"),
            &cancel,
        )
        .await;
        if change == "decline" {
            assert!(cached.is_none());
            assert!(!window.exists());
            continue;
        }
        assert!(cached.is_some());
        assert!(settle_before_install(ready(()), Some(ready(false)), || unreachable!()).await);
        if change == "test marker" {
            std::fs::remove_file(&marker).unwrap();
        }
        let settings = crate::config::IVerge {
            auto_check_update: Some(change != "auto check"),
            auto_install_update: Some(change == "auto install"),
            ..Default::default()
        };
        if change == "cancel" {
            cancel.cancel();
        }
        let mut calls = 0;
        let installed = install_startup_cache(
            &settings,
            &cancel,
            || Ok(!marker.is_file()),
            || {
                install_with_window(
                    || WindowAfterUpdate::ask(Some(window.clone()), None),
                    || {
                        calls += 1;
                        assert!(window.is_file());
                        anyhow::ensure!(change != "error", "installer failed");
                        Ok(())
                    },
                )
            },
        );
        let allowed = matches!(change, "none" | "error")
            || (change == "auto install" && !cfg!(any(windows, target_os = "linux")));
        if change == "error" {
            assert!(installed.is_err());
        } else {
            assert_eq!(installed.unwrap(), allowed);
        }
        assert_eq!(calls, usize::from(allowed));
        assert_eq!(window.is_file(), allowed && change != "error");
        if window.is_file() {
            std::fs::remove_file(&window).unwrap();
        }
    }
    std::fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn only_a_still_offered_cache_can_be_installed_after_confirmation() {
    for (before, after, expected) in [
        (None, Some("A"), None),
        (Some("A"), None, None),
        (
            Some("first verification"),
            Some("second verification"),
            Some("second verification"),
        ),
    ] {
        let offered = Cell::new(before);
        let asked = Cell::new(false);
        let cache = confirmed_cache(
            || ready(offered.get()),
            async {
                asked.set(true);
                offered.set(after);
                true
            },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(asked.get(), before.is_some());
        assert_eq!(cache, expected);
    }
}
