use crate::DownloadEvent;
use std::{
    io::SeekFrom,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_compression::tokio::write::{GzipDecoder as WGzipDecoder, XzDecoder as WXzDecoder};
use oma_console::debug;
use oma_utils::url_no_escape::url_no_escape;
use reqwest::{
    header::{HeaderValue, ACCEPT_RANGES, CONTENT_LENGTH, RANGE},
    Client, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::{
    checksum::Checksum, DownloadEntry, DownloadError, DownloadResult, DownloadSourceType, Summary,
};

/// Downlaod file with retry
pub(crate) async fn try_download<F>(
    client: &Client,
    entry: &DownloadEntry,
    progress: (usize, usize, Option<String>),
    count: usize,
    retry_times: usize,
    context: Option<String>,
    callback: F,
    global_progress: Arc<AtomicU64>,
) -> DownloadResult<Summary>
where
    F: Fn(usize, DownloadEvent) + Clone,
{
    let mut sources = entry.source.clone();
    sources.sort_by(|a, b| a.source_type.cmp(&b.source_type));

    let mut res = None;

    for (i, c) in sources.iter().enumerate() {
        let download_res = match c.source_type {
            DownloadSourceType::Http => {
                try_http_download(
                    client,
                    entry,
                    progress.clone(),
                    count,
                    retry_times,
                    context.clone(),
                    i,
                    callback.clone(),
                    global_progress.clone(),
                )
                .await
            }
            DownloadSourceType::Local => {
                download_local(
                    entry,
                    progress.clone(),
                    count,
                    context.clone(),
                    i,
                    callback.clone(),
                    global_progress.clone(),
                )
                .await
            }
        };

        match download_res {
            Ok(download_res) => {
                res = Some(download_res);
                break;
            }
            Err(e) => {
                if i == sources.len() - 1 {
                    return Err(e);
                }
                callback(count, DownloadEvent::CanNotGetSourceNextUrl(e.to_string()));
            }
        }
    }

    Ok(res.unwrap())
}

/// Downlaod file with retry (http)
async fn try_http_download<F>(
    client: &Client,
    entry: &DownloadEntry,
    progress: (usize, usize, Option<String>),
    count: usize,
    retry_times: usize,
    context: Option<String>,
    position: usize,
    callback: F,
    global_progress: Arc<AtomicU64>,
) -> DownloadResult<Summary>
where
    F: Fn(usize, DownloadEvent) + Clone,
{
    let mut times = 0;
    loop {
        match http_download(
            client,
            entry,
            progress.clone(),
            count,
            context.clone(),
            position,
            callback.clone(),
            global_progress.clone(),
        )
        .await
        {
            Ok(s) => {
                return Ok(s);
            }
            Err(e) => match e {
                DownloadError::ChecksumMisMatch(ref filename, _) => {
                    if retry_times == times {
                        return Err(e);
                    }
                    callback(
                        count,
                        DownloadEvent::ChecksumMismatchRetry {filename: filename.clone(), times }
                    );
                    times += 1;
                }
                _ => return Err(e),
            },
        }
    }
}

/// Download http file
async fn http_download<F>(
    client: &Client,
    entry: &DownloadEntry,
    progress: (usize, usize, Option<String>),
    list_count: usize,
    context: Option<String>,
    position: usize,
    callback: F,
    global_progress: Arc<AtomicU64>,
) -> DownloadResult<Summary>
where
    F: Fn(usize, DownloadEvent) + Clone,
{
    let file = entry.dir.join(&entry.filename);
    let file_exist = file.exists();
    let mut file_size = file.metadata().ok().map(|x| x.len()).unwrap_or(0);

    debug!("Exist file size is: {file_size}");
    let mut dest = None;
    let mut validator = None;

    // 如果要下载的文件已经存在，则验证 Checksum 是否正确，若正确则添加总进度条的进度，并返回
    // 如果不存在，则继续往下走
    if file_exist {
        debug!(
            "File: {} exists, oma will checksum this file.",
            entry.filename
        );
        let hash = entry.hash.clone();
        if let Some(hash) = hash {
            debug!("Hash exist! It is: {hash}");

            let mut f = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(&file)
                .await?;

            debug!(
                "oma opened file: {} with create, write and read mode",
                entry.filename
            );

            let mut v = Checksum::from_sha256_str(&hash)?.get_validator();

            debug!("Validator created.");

            let mut buf = vec![0; 4096];
            let mut readed = 0;

            loop {
                if readed == file_size {
                    break;
                }

                let readed_count = f.read(&mut buf[..]).await?;
                v.update(&buf[..readed_count]);

                global_progress.fetch_add(readed_count as u64, Ordering::SeqCst);

                callback(list_count, DownloadEvent::GlobalProgressInc(readed_count as u64));

                readed += readed_count as u64;
            }

            if v.finish() {
                debug!(
                    "{} checksum success, no need to download anything.",
                    entry.filename
                );

                callback(list_count, DownloadEvent::ProgressDone);

                return Ok(Summary::new(&entry.filename, false, list_count, context));
            }

            debug!("checksum fail, will download this file: {}", entry.filename);

            if !entry.allow_resume {
                global_progress.fetch_sub(readed, Ordering::SeqCst);
                callback(
                    list_count,
                    DownloadEvent::GlobalProgressSet(
                        global_progress.fetch_sub(readed, Ordering::SeqCst),
                    ),
                );
            } else {
                dest = Some(f);
                validator = Some(v);
            }
        }
    }

    let (count, len, msg) = progress;
    let msg = msg.unwrap_or(entry.filename.clone());
    let msg = format!("({count}/{len}) {msg}");
    callback(list_count, DownloadEvent::NewProgressSpinner(msg.clone()));

    let url = entry.source[position].url.clone();
    let resp_head = client.head(url).send().await?;

    let head = resp_head.headers();

    // 看看头是否有 ACCEPT_RANGES 这个变量
    // 如果有，而且值不为 none，则可以断点续传
    // 反之，则不能断点续传
    let mut can_resume = match head.get(ACCEPT_RANGES) {
        Some(x) if x == "none" => false,
        Some(_) => true,
        None => false,
    };

    debug!("Can resume? {can_resume}");

    // 从服务器获取文件的总大小
    let total_size = {
        let total_size = head
            .get(CONTENT_LENGTH)
            .map(|x| x.to_owned())
            .unwrap_or(HeaderValue::from(0));

        total_size
            .to_str()
            .ok()
            .and_then(|x| x.parse::<u64>().ok())
            .unwrap_or_default()
    };

    debug!("File total size is: {total_size}");

    let url = entry.source[position].url.clone();
    let mut req = client.get(url);

    if can_resume && entry.allow_resume {
        // 如果已存在的文件大小大于或等于要下载的文件，则重置文件大小，重新下载
        // 因为已经走过一次 chekcusm 了，函数走到这里，则说明肯定文件完整性不对
        if total_size <= file_size {
            debug!("Exist file size is reset to 0, because total size <= exist file size");
            let gp = global_progress.load(Ordering::SeqCst);
            callback(list_count, DownloadEvent::GlobalProgressSet(gp - file_size));
            global_progress.store(gp - file_size, Ordering::SeqCst);
            file_size = 0;
            can_resume = false;
        }

        // 发送 RANGE 的头，传入的是已经下载的文件的大小
        debug!("oma will set header range as bytes={file_size}-");
        req = req.header(RANGE, format!("bytes={}-", file_size));
    }

    debug!("Can resume? {can_resume}");

    let resp = req.send().await?;

    if let Err(e) = resp.error_for_status_ref() {
        callback(list_count, DownloadEvent::ProgressDone);
        match e.status() {
            Some(StatusCode::NOT_FOUND) => {
                let url = entry.source[position].url.clone();
                return Err(DownloadError::NotFound(url));
            }
            _ => return Err(e.into()),
        }
    } else {
        callback(list_count, DownloadEvent::ProgressDone);
    }

    callback(
        list_count,
        DownloadEvent::NewProgress(total_size, msg.clone()),
    );

    let mut source = resp;

    // 初始化 checksum 验证器
    // 如果文件存在，则 checksum 验证器已经初试过一次，因此进度条加已经验证过的文件大小
    let hash = entry.hash.clone();
    let mut validator = if let Some(v) = validator {
        callback(
            list_count,
            DownloadEvent::ProgressInc(file_size),
        );
        Some(v)
    } else if let Some(hash) = hash {
        Some(Checksum::from_sha256_str(&hash)?.get_validator())
    } else {
        None
    };

    let mut dest = if !entry.allow_resume || !can_resume {
        // 如果不能 resume，则加入 truncate 这个 flag，告诉内核截断文件
        // 并把文件长度设置为 0
        debug!(
            "oma will open file: {} as truncate, create, write and read mode.",
            entry.filename
        );
        let f = tokio::fs::OpenOptions::new()
            .truncate(true)
            .create(true)
            .write(true)
            .read(true)
            .open(&file)
            .await?;

        debug!("Setting file length as 0");
        f.set_len(0).await?;

        f
    } else if let Some(dest) = dest {
        debug!("oma will re use opened dest file for {}", entry.filename);

        dest
    } else {
        debug!(
            "oma will open file: {} as create, write and read mode.",
            entry.filename
        );

        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&file)
            .await?
    };

    // 把文件指针移动到末尾
    debug!("oma will seek file: {} to end", entry.filename);
    dest.seek(SeekFrom::End(0)).await?;

    let mut writer: Box<dyn AsyncWrite + Unpin + Send> =
        match Path::new(&entry.source[position].url)
            .extension()
            .and_then(|x| x.to_str())
        {
            Some("xz") if entry.extract => Box::new(WXzDecoder::new(&mut dest)),
            Some("gz") if entry.extract => Box::new(WGzipDecoder::new(&mut dest)),
            _ => Box::new(&mut dest),
        };

    // 下载！
    debug!("Start download!");
    let mut self_progress = 0;
    while let Some(chunk) = source.chunk().await? {
        writer.write_all(&chunk).await?;
        callback(
            list_count,
            DownloadEvent::ProgressInc(chunk.len() as u64),
        );
        self_progress += chunk.len() as u64;

        callback(list_count, DownloadEvent::GlobalProgressInc(chunk.len() as u64));
        global_progress.store(
            global_progress.load(Ordering::SeqCst) + chunk.len() as u64,
            Ordering::SeqCst,
        );

        if let Some(ref mut v) = validator {
            v.update(&chunk);
        }
    }

    // 下载完成，告诉内核不再写这个文件了
    debug!("Download complete! shutting down dest file stream ...");
    writer.shutdown().await?;

    // 最后看看 chekcsum 验证是否通过
    if let Some(v) = validator {
        if !v.finish() {
            debug!("checksum fail: {}", entry.filename);

            callback(
                list_count,
                DownloadEvent::GlobalProgressSet(
                    global_progress.load(Ordering::SeqCst) - self_progress,
                ),
            );
            global_progress.store(
                global_progress.load(Ordering::SeqCst) - self_progress,
                Ordering::SeqCst,
            );

            let url = entry.source[position].url.clone();
            return Err(DownloadError::ChecksumMisMatch(
                url,
                entry.dir.display().to_string(),
            ));
        }

        debug!("checksum success: {}", entry.filename);
    }

    callback(list_count, DownloadEvent::ProgressDone);

    Ok(Summary::new(&entry.filename, true, list_count, context))
}

/// Download local source file
pub async fn download_local<F>(
    entry: &DownloadEntry,
    progress: (usize, usize, Option<String>),
    list_count: usize,
    context: Option<String>,
    position: usize,
    callback: F,
    global_progress: Arc<AtomicU64>,
) -> DownloadResult<Summary>
where
    F: Fn(usize, DownloadEvent) + Clone,
{
    debug!("{entry:?}");
    let (c, len, msg) = progress;
    let msg = msg.unwrap_or(entry.filename.clone());
    let msg = format!("({c}/{len}) {msg}");
    callback(list_count, DownloadEvent::NewProgressSpinner(msg.clone()));

    let url = &entry.source[position].url;
    let url_path = url_no_escape(
        url.strip_prefix("file:")
            .ok_or_else(|| DownloadError::InvaildURL(url.to_string()))?,
    );

    debug!("File path is: {url_path}");

    let mut from = tokio::fs::File::open(&url_path).await.map_err(|e| {
        DownloadError::FailedOpenLocalSourceFile(entry.filename.clone(), e.to_string())
    })?;

    debug!("Success open file: {url_path}");

    let mut to = tokio::fs::File::create(entry.dir.join(&entry.filename))
        .await
        .map_err(|e| {
            DownloadError::FailedOpenLocalSourceFile(entry.filename.clone(), e.to_string())
        })?;

    debug!(
        "Success create file: {}",
        entry.dir.join(&entry.filename).display()
    );

    let size = tokio::io::copy(&mut from, &mut to).await.map_err(|e| {
        DownloadError::FailedOpenLocalSourceFile(entry.filename.clone(), e.to_string())
    })?;

    debug!(
        "Success copy file from {url_path} to {}",
        entry.dir.join(&entry.filename).display()
    );

    callback(list_count, DownloadEvent::ProgressDone);
    callback(list_count, DownloadEvent::GlobalProgressInc(size));
    global_progress.fetch_add(size, Ordering::SeqCst);

    Ok(Summary::new(&entry.filename, true, c, context))
}
