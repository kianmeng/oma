use std::borrow::Cow;

use crate::fl;
use crate::history::connect_db;
use crate::history::write_history_entry;
use crate::history::SummaryLog;
use crate::history::SummaryType;
use crate::pb;
use crate::table::table_for_install_pending;
use crate::utils::create_async_runtime;
use crate::utils::multibar;
use crate::Result;
use chrono::TimeZone;
use chrono::Utc;
use dialoguer::console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::Select;
use oma_console::error;
use oma_console::info;
use oma_console::success;
use oma_console::warn;
use oma_console::WRITER;
use oma_fetch::DownloadEvent;
use oma_pm::apt::AptArgs;
use oma_pm::apt::OmaApt;
use oma_pm::operation::InstallEntry;
use oma_pm::operation::RemoveEntry;
use oma_refresh::db::OmaRefresh;
use oma_refresh::db::RefreshEvent;

pub(crate) fn handle_no_result(no_result: Vec<String>) {
    for word in no_result {
        error!("{}", fl!("could-not-find-pkg-from-keyword", c = word));
    }
}

pub(crate) fn refresh(dry_run: bool, no_progress: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }

    info!("{}", fl!("refreshing-repo-metadata"));
    let refresh = OmaRefresh::scan(None)?;
    let tokio = create_async_runtime()?;

    let (mb, pb_map, global_is_set) = multibar();

    let pbc = pb_map.clone();

    tokio.block_on(async move {
        refresh
            .start(|count, event, total| {
                if !no_progress {
                    match event {
                        RefreshEvent::ClosingTopic(topic_name) => {
                            WRITER
                                .writeln_with_mb(
                                    &mb,
                                    &style("INFO").blue().bold().to_string(),
                                    &fl!("scan-topic-is-removed", name = topic_name),
                                )
                                .ok();
                        }
                        RefreshEvent::DownloadEvent(event) => {
                            pb!(event, mb, pb_map, count, total, global_is_set)
                        }
                    }
                } else {
                    match event {
                        RefreshEvent::DownloadEvent(d) => {
                            handle_event_without_progressbar(d);
                        }
                        RefreshEvent::ClosingTopic(topic_name) => {
                            info!("{}", fl!("scan-topic-is-removed", name = topic_name));
                        }
                    }
                }
            })
            .await
    })?;

    if let Some(gpb) = pbc.get(&0) {
        gpb.finish_and_clear();
    }

    Ok(())
}

pub(crate) fn normal_commit(
    apt: OmaApt,
    dry_run: bool,
    typ: SummaryType,
    apt_args: AptArgs,
    no_fixbroken: bool,
    network_thread: usize,
    no_progress: bool,
) -> Result<()> {
    let op = apt.summary()?;
    let op_after = op.clone();
    let install = op.install;
    let remove = op.remove;
    let disk_size = op.disk_size;
    if check_empty_op(&install, &remove) {
        return Ok(());
    }

    apt.resolve(no_fixbroken)?;
    apt.check_disk_size()?;

    table_for_install_pending(
        &install,
        &remove,
        &disk_size,
        !apt_args.yes(),
        dry_run,
        !apt_args.yes(),
    )?;

    let (mb, pb_map, global_is_set) = multibar();

    let start_time = apt.commit(Some(network_thread), &apt_args, |count, event, total| {
        if !no_progress {
            pb!(event, mb, pb_map, count, total, global_is_set)
        } else {
            handle_event_without_progressbar(event);
        }
    })?;

    write_history_entry(op_after, typ, connect_db(true)?, dry_run, start_time)?;

    Ok(())
}

pub(crate) fn handle_event_without_progressbar(event: DownloadEvent) {
    match event {
        DownloadEvent::ChecksumMismatchRetry { filename, times } => {
            error!(
                "{}",
                fl!("checksum-mismatch-retry", c = filename, retry = times)
            );
        }
        DownloadEvent::CanNotGetSourceNextUrl(e) => {
            error!("{}", fl!("can-not-get-source-next-url", e = e.to_string()));
        }
        DownloadEvent::Done(msg) => {
            WRITER.writeln("DONE", &msg, false).ok();
        }
        _ => {}
    }
}

pub(crate) fn check_empty_op(install: &[InstallEntry], remove: &[RemoveEntry]) -> bool {
    if install.is_empty() && remove.is_empty() {
        success!("{}", fl!("no-need-to-do-anything"));
        return true;
    }

    false
}

pub(crate) fn dialoguer_select_history(
    display_list: &[String],
    old_selected: usize,
) -> Result<usize> {
    let selected = Select::with_theme(&ColorfulTheme::default())
        .items(display_list)
        .default(old_selected)
        .interact()?;

    Ok(selected)
}

pub(crate) fn format_summary_log(list: &[(SummaryLog, u64)], undo: bool) -> Vec<String> {
    let display_list = list
        .iter()
        .filter(|(log, _)| {
            if undo {
                log.typ != SummaryType::FixBroken
            } else {
                true
            }
        })
        .map(|(log, date)| {
            let date = format_date(*date);
            match &log.typ {
                SummaryType::Install(v) if v.len() > 3 => format!(
                    "Installed {} {} {} ... (and {} more) [{}]",
                    v[0],
                    v[1],
                    v[2],
                    v.len() - 3,
                    date
                ),
                SummaryType::Install(v) => format!("Installl {} [{date}]", v.join(" ")),
                SummaryType::Upgrade(v) if v.is_empty() => format!("Upgraded system [{date}]"),
                SummaryType::Upgrade(v) if v.len() > 3 => format!(
                    "Upgraded system and installed {} {} {} ... (and {} more) [{date}]",
                    v[0],
                    v[1],
                    v[2],
                    v.len() - 3
                ),
                SummaryType::Upgrade(v) => {
                    format!("Upgraded system and install {} [{date}]", v.join(" "))
                }
                SummaryType::Remove(v) if v.len() > 3 => format!(
                    "Removed {} {} {} ... (and {} more)",
                    v[0],
                    v[1],
                    v[2],
                    v.len() - 3
                ),
                SummaryType::Remove(v) => format!("Removed {} [{date}]", v.join(" ")),
                SummaryType::FixBroken => format!("Attempted to fix broken dependencies [{date}]"),
                SummaryType::TopicsChanged { add, remove } if remove.is_empty() => {
                    format!(
                        "Topics changed: enabled {}{} [{date}]",
                        if add.len() <= 3 {
                            add.join(" ")
                        } else {
                            format!("{} {} {}", add[0], add[1], add[2])
                        },
                        if add.len() <= 3 {
                            Cow::Borrowed("")
                        } else {
                            Cow::Owned(format!(" ... (and {} more)", add.len() - 3))
                        }
                    )
                }
                SummaryType::TopicsChanged { add, remove } if add.is_empty() => {
                    format!(
                        "Topics changed: disabled {}{} [{date}]",
                        if remove.len() <= 3 {
                            add.join(" ")
                        } else {
                            format!("{} {} {}", remove[0], remove[1], remove[2])
                        },
                        if remove.len() <= 3 {
                            Cow::Borrowed("")
                        } else {
                            Cow::Owned(format!(" ... (and {} more)", remove.len() - 3))
                        }
                    )
                }
                SummaryType::TopicsChanged { add, remove } => {
                    format!(
                        "Topics changed: enabled {}{}, disabled {}{} [{date}]",
                        if add.len() <= 3 {
                            add.join(" ")
                        } else {
                            format!("{} {} {}", add[0], add[1], add[2])
                        },
                        if add.len() <= 3 {
                            Cow::Borrowed("")
                        } else {
                            Cow::Owned(format!(" ... (and {} more)", add.len() - 3))
                        },
                        if remove.len() <= 3 {
                            remove.join(" ")
                        } else {
                            format!("{} {} {}", remove[0], remove[1], remove[2])
                        },
                        if remove.len() <= 3 {
                            Cow::Borrowed("")
                        } else {
                            Cow::Owned(format!(" ... (and {} more)", add.len() - 3))
                        },
                    )
                }
                SummaryType::Undo => format!("Undone [{date}]"),
            }
        })
        .collect::<Vec<_>>();

    display_list
}

pub(crate) fn check_unsupport_stmt(s: &str) {
    let unsupport = vec!['=', '/'];
    for i in unsupport {
        if s.contains(i) {
            warn!("Unexpected pattern: {s}");
        }
    }
}

fn format_date(date: u64) -> String {
    let dt = Utc.timestamp_opt(date as i64, 0).unwrap().naive_local();
    let s = dt.format("%H:%M:%S on %Y-%m-%d").to_string();

    s
}