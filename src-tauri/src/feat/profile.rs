use crate::core::notify::NoticeStatus;
use crate::core::notify::{Refresh, announce};
use crate::{
    cmd,
    config::{Config, PrfItem, PrfOption, profiles::profiles_update_item_safe},
    core::{
        CoreManager, handle, tray,
        validate::{ValidationErrorKind, ValidationOutcome, ValidationSkipReason},
    },
    utils::{
        dirs,
        help::{self, mask_err, mask_url},
    },
};
use anyhow::{Result, bail};
use clash_verge_logging::{Type, logging};
use smartstring::alias::String;
use std::path::PathBuf;

/// Toggle proxy profile
pub async fn toggle_proxy_profile(profile_index: String) {
    if let Err(err) = cmd::patch_profiles_config_by_profile_index(profile_index.clone()).await {
        logging!(
            error,
            Type::Config,
            "toggle proxy profile {profile_index} failed: {err}"
        );
    }
}

/// Tell the profile which node this group is on now.
///
/// The core is already switched by the time this runs, so failing to record is not a reason to
/// report the switch as failed — but it is worth a log line, because what the profile holds is
/// what gets re-applied the next time a core starts.
async fn record_switched_node(group_name: &str, proxy_name: &str) {
    if let Err(error) = crate::config::profiles::record_selected_node(group_name, proxy_name).await {
        logging!(
            warn,
            Type::Config,
            "切换代理成功但未能记录到配置: {} -> {}, 错误: {error:#}",
            group_name,
            proxy_name
        );
    }
}

pub async fn switch_proxy_node(group_name: &str, proxy_name: &str) {
    match handle::Handle::mihomo()
        .select_node_for_group(group_name, proxy_name)
        .await
    {
        Ok(_) => {
            record_switched_node(group_name, proxy_name).await;
            announce(Refresh::Proxies);
            let _ = tray::Tray::global().update_menu().await;
            return;
        }
        Err(err) => {
            logging!(error, Type::Tray, "切换代理失败: {err:?}");
        }
    }

    match handle::Handle::mihomo()
        .select_node_for_group(group_name, proxy_name)
        .await
    {
        Ok(_) => {
            logging!(info, Type::Tray, "代理切换回退成功: {} -> {}", group_name, proxy_name);
            record_switched_node(group_name, proxy_name).await;
            let _ = tray::Tray::global().update_menu().await;
        }
        Err(err) => {
            logging!(error, Type::Tray, "代理切换最终失败: {err:?}");
        }
    }
}

async fn should_update_profile(uid: &String, ignore_auto_update: bool) -> Result<Option<(String, Option<PrfOption>)>> {
    let profiles = Config::profiles().await;
    let profiles = profiles.latest_arc();
    let item = profiles.get_item(uid)?;
    let is_remote = item.itype.as_ref().is_some_and(|s| s == "remote");

    if !is_remote {
        logging!(info, Type::Config, "[订阅更新] 不是远程订阅，跳过更新");
        Ok(None)
    } else if item.url.is_none() {
        logging!(warn, Type::Config, "[订阅更新] 缺少URL，无法更新");
        bail!("failed to get the profile item url");
    } else if !ignore_auto_update && !item.option.as_ref().and_then(|o| o.allow_auto_update).unwrap_or(true) {
        logging!(info, Type::Config, "[订阅更新] 禁止自动更新，跳过更新");
        Ok(None)
    } else {
        logging!(
            info,
            Type::Config,
            "[订阅更新] 远程订阅，URL: {}",
            mask_url(
                item.url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Profile URL is None"))?
            )
        );
        Ok(Some((
            item.url.clone().ok_or_else(|| anyhow::anyhow!("Profile URL is None"))?,
            item.option.clone(),
        )))
    }
}

#[tracing::instrument(skip_all, level = "info", fields(uid = %uid, strategy = tracing::field::Empty, configured_elapsed = tracing::field::Empty, clash_elapsed = tracing::field::Empty, system_elapsed = tracing::field::Empty))]
async fn perform_profile_update(
    uid: &String,
    url: &String,
    opt: Option<&PrfOption>,
    option: Option<&PrfOption>,
    is_mannual_trigger: bool,
) -> Result<Option<ReplacedFile>> {
    let merged_opt = PrfOption::merge(opt, option);
    let profiles = Config::profiles().await;
    let profiles_arc = profiles.latest_arc();
    let profile_name = profiles_arc
        .get_item(uid)
        .ok()
        .and_then(|item| item.name.clone())
        .unwrap_or_else(|| String::from("UnKnown Profile"));

    #[derive(Clone, Copy)]
    enum Strategy {
        Configured,
        Clash,
        System,
    }

    let result =
        crate::utils::retry::try_strategies(Strategy::Configured, [Strategy::Clash, Strategy::System], |strategy| {
            let mut options = merged_opt.clone();
            async move {
                let (name, success, failure) = match strategy {
                    Strategy::Configured => (
                        "configured",
                        "[订阅更新] 更新订阅配置成功",
                        "[订阅更新] 直接更新失败: {}，尝试使用Clash代理更新",
                    ),
                    Strategy::Clash => (
                        "clash",
                        "[订阅更新] 使用 Clash代理 更新订阅配置成功",
                        "[订阅更新] Clash代理更新失败: {}，尝试使用系统代理更新",
                    ),
                    Strategy::System => (
                        "system",
                        "[订阅更新] 使用 系统代理 更新订阅配置成功",
                        "[订阅更新] 系统代理更新失败: {}，所有重试均已失败",
                    ),
                };
                match strategy {
                    Strategy::Configured => {}
                    Strategy::Clash | Strategy::System => {
                        let option = options.get_or_insert_with(PrfOption::default);
                        option.self_proxy = Some(matches!(strategy, Strategy::Clash));
                        option.with_proxy = Some(matches!(strategy, Strategy::System));
                    }
                }
                tracing::Span::current().record("strategy", name);
                let started = std::time::Instant::now();
                let result = PrfItem::from_url(url, None, None, options.as_ref()).await;
                let elapsed_field = match strategy {
                    Strategy::Configured => "configured_elapsed",
                    Strategy::Clash => "clash_elapsed",
                    Strategy::System => "system_elapsed",
                };
                tracing::Span::current().record(elapsed_field, tracing::field::debug(started.elapsed()));
                match &result {
                    Ok(_) => logging!(info, Type::Config, "{success}"),
                    Err(error) => logging!(
                        warn,
                        Type::Config,
                        "{}",
                        failure.replace("{}", &mask_err(&format!("{error:#}")))
                    ),
                }
                result
            }
        })
        .await;
    let last_err = match result {
        Ok((strategy, mut item)) => {
            // A persistence error must not replay a successful download with another transport.
            let replaced = save_downloaded_profile(uid, &mut item).await?;
            if !matches!(strategy, Strategy::Configured) {
                handle::Handle::notice(NoticeStatus::UpdateWithClashProxy, profile_name.as_str());
            }
            return Ok(replaced);
        }
        Err(error) => error,
    };

    let last_err = mask_err(&last_err.to_string());
    if is_mannual_trigger {
        handle::Handle::notice(
            NoticeStatus::UpdateFailedEvenWithClash,
            format!("{profile_name} - {last_err}"),
        );
    }
    bail!(last_err)
}

#[tracing::instrument(skip_all, level = "info", fields(uid = %uid, manual = is_mannual_trigger))]
pub async fn update_profile(uid: &String, option: Option<&PrfOption>, is_mannual_trigger: bool) -> Result<()> {
    let url_opt = should_update_profile(uid, is_mannual_trigger).await?;

    let mut replaced = None;
    let profile_persisted = match url_opt {
        Some((url, opt)) => {
            replaced = perform_profile_update(uid, &url, opt.as_ref(), option, is_mannual_trigger).await?;
            true
        }
        None => false,
    };
    let profiles = Config::profiles().await;
    let is_current = profiles.latest_arc().current.as_ref() == Some(uid);
    let should_refresh = is_current || (!profile_persisted && is_mannual_trigger);

    if should_refresh {
        logging!(debug, Type::Config, "[订阅更新] 更新内核配置");
        let mut result = CoreManager::global().update_config_with_force(is_mannual_trigger).await;
        if profile_persisted && !is_mannual_trigger {
            result = apply_saved_profile(uid, result).await;
        }
        // the outcome is for whichever profile is current when the config is generated
        let restored = profile_persisted
            && rejects_content(&result)
            && is_current_profile(uid).await
            && restore_profile_file(replaced).await;
        match result {
            Ok(outcome) if outcome.is_valid() => announce(Refresh::Clash),
            Ok(outcome @ (ValidationOutcome::Skipped { .. } | ValidationOutcome::Busy)) if !is_mannual_trigger => {
                logging!(info, Type::Config, "[订阅更新] 本次配置刷新已跳过: {}", outcome);
            }
            result => {
                let message = match result {
                    Ok(outcome) => outcome.to_string(),
                    Err(err) => err.to_string(),
                };
                let message = mask_err(&message);
                let message = if restored {
                    format!("订阅新内容未通过校验，已保留原订阅: {message}")
                } else if profile_persisted {
                    format!("订阅已保存，但未能确认已应用到内核: {message}")
                } else {
                    message
                };
                logging!(error, Type::Config, "[订阅更新] 更新失败: {}", message);
                handle::Handle::notice(NoticeStatus::UpdateFailed, message.as_str());
                bail!(message);
            }
        }
    }

    Ok(())
}

/// A subscription file an update replaced, with what the update wrote over it.
struct ReplacedFile {
    path: PathBuf,
    previous: String,
    written: String,
}

/// Saves a downloaded subscription, keeping the content it replaces.
async fn save_downloaded_profile(uid: &String, item: &mut PrfItem) -> Result<Option<ReplacedFile>> {
    let _profile_write = crate::config::profiles::PROFILE_WRITE_LOCK.lock().await;
    let previous = saved_profile_file(uid).await;
    let written = item.file_data.clone();
    profiles_update_item_safe(uid, item).await?;
    Ok(previous.zip(written).map(|((path, previous), written)| ReplacedFile {
        path,
        previous,
        written,
    }))
}

async fn is_current_profile(uid: &String) -> bool {
    Config::profiles().await.latest_arc().current.as_ref() == Some(uid)
}

/// The file a subscription is saved in, with its content.
async fn saved_profile_file(uid: &String) -> Option<(PathBuf, String)> {
    let file = Config::profiles().await.latest_arc().get_item(uid).ok()?.file.clone()?;
    let path = dirs::app_profiles_dir().ok()?.join(file.as_str());
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    Some((path, content.into()))
}

/// A download can bring content the core rejects, such as an empty or invalid node list,
/// which would replace the working copy and leave no proxies after the next restart.
const fn rejects_content(result: &Result<ValidationOutcome>) -> bool {
    matches!(
        result,
        Ok(ValidationOutcome::Invalid {
            kind: ValidationErrorKind::YamlSyntax
                | ValidationErrorKind::YamlMapping
                | ValidationErrorKind::CoreRejected,
            ..
        })
    )
}

async fn restore_profile_file(replaced: Option<ReplacedFile>) -> bool {
    let Some(ReplacedFile {
        path,
        previous,
        written,
    }) = replaced
    else {
        return false;
    };
    let _profile_write = crate::config::profiles::PROFILE_WRITE_LOCK.lock().await;
    // another update or an edit has replaced the content since
    if tokio::fs::read_to_string(&path).await.ok().as_deref() != Some(written.as_str()) {
        return false;
    }
    match help::save_yaml_str(&path, &previous).await {
        Ok(()) => true,
        Err(err) => {
            logging!(error, Type::Config, "[订阅更新] 恢复原订阅失败: {err:#}");
            false
        }
    }
}

const APPLY_SAVED_PROFILE_ATTEMPTS: usize = 60;
const APPLY_SAVED_PROFILE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Applies a saved subscription that a config update running meanwhile, or the
/// debounce right after one, kept from the core: that update may have read the
/// previous subscription, and the next automatic update is hours away.
async fn apply_saved_profile(uid: &String, mut result: Result<ValidationOutcome>) -> Result<ValidationOutcome> {
    let kept_from_core = |result: &Result<ValidationOutcome>| {
        matches!(
            result,
            Ok(ValidationOutcome::Busy
                | ValidationOutcome::Skipped {
                    reason: ValidationSkipReason::Debounced,
                })
        )
    };
    for _ in 0..APPLY_SAVED_PROFILE_ATTEMPTS {
        // switching to another profile applies that one itself
        if !kept_from_core(&result) || !is_current_profile(uid).await {
            return result;
        }
        tokio::time::sleep(APPLY_SAVED_PROFILE_RETRY_DELAY).await;
        result = CoreManager::global().update_config_with_force(true).await;
    }
    // reported as not applied rather than skipped, which would count as done
    if kept_from_core(&result) {
        bail!("other config updates kept running");
    }
    result
}

/// 增强配置
pub async fn enhance_profiles() -> Result<ValidationOutcome> {
    CoreManager::global().update_config_forced().await
}
