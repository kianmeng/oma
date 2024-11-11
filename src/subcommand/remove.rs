use anyhow::anyhow;
use apt_auth_config::AuthConfig;
use dialoguer::console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};
use oma_history::SummaryType;
use oma_pm::apt::{AptConfig, OmaApt, OmaAptArgs};
use tracing::{info, warn};

use crate::pb::OmaProgressBar;
use crate::{
    error::OutputError,
    utils::{dbus_check, root},
    RemoveArgs,
};
use crate::{fl, OmaArgs, HTTP_CLIENT};

use super::utils::{handle_no_result, lock_oma, no_check_dbus_warn, CommitRequest};

pub fn execute(pkgs: Vec<&str>, args: RemoveArgs, oma_args: OmaArgs) -> Result<i32, OutputError> {
    root()?;
    lock_oma()?;

    let OmaArgs {
        dry_run,
        network_thread,
        no_progress,
        no_check_dbus,
        protect_essentials: protect,
        another_apt_options,
    } = oma_args;

    let fds = if !no_check_dbus {
        Some(dbus_check(args.yes))
    } else {
        no_check_dbus_warn();
        None
    };

    if args.yes {
        warn!("{}", fl!("automatic-mode-warn"));
    }

    let oma_apt_args = OmaAptArgs::builder()
        .yes(args.yes)
        .force_yes(args.force_yes)
        .no_progress(no_progress)
        .sysroot(args.sysroot.clone())
        .another_apt_options(another_apt_options)
        .dpkg_force_unsafe_io(args.force_unsafe_io)
        .build();

    let mut apt = OmaApt::new(vec![], oma_apt_args, dry_run, AptConfig::new())?;
    let (pkgs, no_result) = apt.select_pkg(&pkgs, false, false, false)?;

    let pb = if !no_progress {
        OmaProgressBar::new_spinner(Some(fl!("resolving-dependencies"))).into()
    } else {
        None
    };

    let context = apt.remove(&pkgs, args.remove_config, args.no_autoremove)?;

    if let Some(pb) = pb {
        pb.inner.finish_and_clear()
    }

    if !context.is_empty() {
        for c in context {
            info!("{}", fl!("no-need-to-remove", name = c));
        }
    }

    handle_no_result(&args.sysroot, no_result, no_progress)?;

    let auth_config = AuthConfig::system(&args.sysroot)?;

    let request = CommitRequest {
        apt,
        dry_run,
        request_type: SummaryType::Remove(
            pkgs.iter()
                .map(|x| format!("{} {}", x.raw_pkg.fullname(true), x.version_raw.version()))
                .collect::<Vec<_>>(),
        ),
        no_fixbroken: !args.fix_broken,
        network_thread,
        no_progress,
        sysroot: args.sysroot,
        fix_dpkg_status: true,
        protect_essential: protect,
        client: &HTTP_CLIENT,
        yes: args.yes,
        remove_config: args.remove_config,
        auth_config: &auth_config,
    };

    let code = request.run()?;

    drop(fds);

    Ok(code)
}

/// "Yes Do as I say" steps
pub fn ask_user_do_as_i_say(pkg: &str) -> anyhow::Result<bool> {
    let theme = ColorfulTheme::default();

    warn!("{}", fl!("essential-tips", pkg = pkg));

    let delete = Confirm::with_theme(&theme)
        .with_prompt(fl!("essential-continue"))
        .default(false)
        .interact()
        .map_err(|_| anyhow!(""))?;

    if !delete {
        return Ok(false);
    }

    info!(
        "{}",
        fl!(
            "yes-do-as-i-say",
            input = style("Do as I say!").bold().to_string()
        ),
    );

    if Input::<String>::with_theme(&theme)
        .with_prompt(fl!("yes-do-as-i-say-prompt"))
        .interact()?
        != "Do as I say!"
    {
        info!("{}", fl!("yes-do-as-i-say-abort"));
        return Ok(false);
    }

    Ok(true)
}
