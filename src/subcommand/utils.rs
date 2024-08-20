use std::backtrace::Backtrace;
use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::panic;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::error::OutputError;
use crate::fl;
use crate::pb::NoProgressBar;
use crate::pb::OmaProgress;
use crate::pb::OmaProgressBar;
use crate::pb::ProgressEvent;
use crate::table::table_for_install_pending;
use crate::utils::create_async_runtime;
use crate::LOCKED;
use chrono::Local;
use oma_console::success;
use oma_history::connect_db;
use oma_history::create_db_file;
use oma_history::write_history_entry;
use oma_history::SummaryType;
use oma_pm::apt::AptArgs;
use oma_pm::apt::OmaApt;
use oma_pm::apt::{InstallEntry, RemoveEntry};
use oma_refresh::db::OmaRefresh;
use oma_refresh::db::OmaRefreshBuilder;
use oma_utils::dpkg::dpkg_arch;
use oma_utils::oma::lock_oma_inner;
use oma_utils::oma::unlock_oma;
use reqwest::Client;
use std::fmt::Display;
use tracing::error;
use tracing::info;
use tracing::warn;

use super::remove::ask_user_do_as_i_say;

pub(crate) fn handle_no_result(no_result: Vec<String>) -> Result<(), OutputError> {
    for word in &no_result {
        if word == "266" {
            error!("无法找到匹配关键字为艾露露的软件包");
        } else {
            error!("{}", fl!("could-not-find-pkg-from-keyword", c = word));
        }
    }

    if no_result.is_empty() {
        Ok(())
    } else {
        Err(OutputError {
            description: fl!("has-error-on-top"),
            source: None,
        })
    }
}

#[derive(Debug)]
pub struct LockError {
    source: io::Error,
}

impl Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to lock oma")
    }
}

impl Error for LockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn lock_oma() -> Result<(), LockError> {
    lock_oma_inner().map_err(|e| LockError { source: e })?;

    panic::set_hook(Box::new(|info| {
        let backtrace = Backtrace::force_capture();
        eprintln!("{}", info);
        eprintln!("Backtrace:");
        eprintln!("{}", backtrace);
        unlock_oma().ok();
    }));

    LOCKED.store(true, Ordering::Relaxed);

    Ok(())
}

pub(crate) fn refresh(
    client: &Client,
    dry_run: bool,
    no_progress: bool,
    download_pure_db: bool,
    limit: usize,
    sysroot: &str,
    _refresh_topics: bool,
) -> Result<(), OutputError> {
    if dry_run {
        return Ok(());
    }

    info!("{}", fl!("refreshing-repo-metadata"));

    let download_pure_db = if dpkg_arch(sysroot)
        .map(|x| x == "mips64r6el")
        .unwrap_or(false)
    {
        false
    } else {
        download_pure_db
    };

    let sysroot = PathBuf::from(sysroot);

    let refresh: OmaRefresh = OmaRefreshBuilder {
        source: sysroot.clone(),
        limit: Some(limit),
        arch: dpkg_arch(&sysroot)?,
        download_dir: sysroot.join("var/lib/apt/lists"),
        download_compress: !download_pure_db,
        client,
        #[cfg(feature = "aosc")]
        refresh_topics: _refresh_topics,
    }
    .into();

    let tokio = create_async_runtime()?;

    let mut pb_map_clone = None;

    let oma_pb: Box<dyn OmaProgress + Send + Sync> = if !no_progress {
        let pb = OmaProgressBar::new();
        pb_map_clone = Some(pb.pb_map.clone());
        Box::new(pb)
    } else {
        Box::new(NoProgressBar)
    };

    tokio.block_on(async move {
        refresh
            .start(
                |count, event, total| {
                    oma_pb.change(ProgressEvent::from(event), count, total);
                },
                || format!("{}\n", fl!("do-not-edit-topic-sources-list")),
            )
            .await
    })?;

    if let Some(pb_map) = pb_map_clone {
        if let Some(gpb) = pb_map.get(&0) {
            gpb.finish_and_clear();
        }
    }

    Ok(())
}

pub struct NormalCommitArgs {
    pub apt: OmaApt,
    pub dry_run: bool,
    pub typ: SummaryType,
    pub apt_args: AptArgs,
    pub no_fixbroken: bool,
    pub network_thread: usize,
    pub no_progress: bool,
    pub sysroot: String,
    pub fix_dpkg_status: bool,
    pub protect_essential: bool,
}

pub(crate) fn normal_commit(args: NormalCommitArgs, client: &Client) -> Result<(), OutputError> {
    let NormalCommitArgs {
        mut apt,
        dry_run,
        typ,
        apt_args,
        no_fixbroken,
        network_thread,
        no_progress,
        sysroot,
        fix_dpkg_status,
        protect_essential,
    } = args;

    apt.resolve(no_fixbroken, fix_dpkg_status)?;

    let op = apt.summary(|pkg| {
        if protect_essential {
            false
        } else {
            ask_user_do_as_i_say(pkg).unwrap_or(false)
        }
    })?;

    apt.check_disk_size(&op)?;

    let op_after = op.clone();
    let install = &op.install;
    let remove = &op.remove;
    let disk_size = &op.disk_size;

    if check_empty_op(install, remove) {
        return Ok(());
    }

    table_for_install_pending(install, remove, disk_size, !apt_args.yes(), dry_run)?;

    let oma_pb: Box<dyn OmaProgress + Sync + Send> = if !no_progress {
        let pb = OmaProgressBar::new();
        Box::new(pb)
    } else {
        Box::new(NoProgressBar)
    };

    let start_time = Local::now().timestamp();

    let res = apt.commit(
        client,
        Some(network_thread),
        &apt_args,
        |count, event, total| {
            oma_pb.change(ProgressEvent::from(event), count, total);
        },
        op,
    );

    match res {
        Ok(_) => {
            success!("{}", fl!("history-tips-1"));
            info!("{}", fl!("history-tips-2"));
            write_history_entry(
                op_after,
                typ,
                {
                    let db = create_db_file(sysroot)?;
                    connect_db(db, true)?
                },
                dry_run,
                start_time,
                true,
            )?;
            Ok(())
        }
        Err(e) => {
            info!("{}", fl!("history-tips-2"));
            write_history_entry(
                op_after,
                typ,
                {
                    let db = create_db_file(sysroot)?;
                    connect_db(db, true)?
                },
                dry_run,
                start_time,
                false,
            )?;
            Err(e.into())
        }
    }
}

pub(crate) fn check_empty_op(install: &[InstallEntry], remove: &[RemoveEntry]) -> bool {
    if install.is_empty() && remove.is_empty() {
        success!("{}", fl!("no-need-to-do-anything"));
        return true;
    }

    false
}

pub(crate) fn check_unsupport_stmt(s: &str) {
    for i in s.chars() {
        if !i.is_ascii_alphabetic()
            && !i.is_ascii_alphanumeric()
            && i != '-'
            && i != '.'
            && i != ':'
        {
            warn!("Unexpected pattern: {s}");
        }
    }
}

pub(crate) fn no_check_dbus_warn() {
    warn!("{}", fl!("no-check-dbus-tips"));
}
