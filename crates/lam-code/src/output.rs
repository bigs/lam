use std::io;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::config::CaptureConfig;
use crate::shell::CapturedStream;

pub(crate) async fn capture_stream(
    mut reader: impl AsyncRead + Unpin,
    config: CaptureConfig,
    scratch: &Path,
    label: &str,
) -> io::Result<CapturedStream> {
    let mut buffer = [0u8; 8 * 1024];
    let mut initial = Vec::new();
    let mut tail = Vec::new();
    let mut spill: Option<tokio::fs::File> = None;
    let mut full_output_path = None;
    let mut total_bytes = 0u64;
    let mut newline_count = 0u64;
    let mut has_output = false;
    let mut ends_with_newline = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        has_output = true;
        ends_with_newline = chunk.ends_with(b"\n");
        total_bytes = total_bytes.saturating_add(read as u64);
        newline_count = newline_count
            .saturating_add(chunk.iter().filter(|byte| **byte == b'\n').count() as u64);

        tail.extend_from_slice(chunk);
        trim_rolling_tail(&mut tail, config.max_bytes.saturating_mul(2));

        let total_lines = newline_count + u64::from(has_output && !ends_with_newline);
        let exceeded =
            total_bytes > config.max_bytes as u64 || total_lines > config.max_lines as u64;
        if spill.is_none() && exceeded {
            let (mut file, path) = create_spill_file(scratch, label)?;
            file.write_all(&initial).await?;
            file.write_all(chunk).await?;
            initial.clear();
            spill = Some(file);
            full_output_path = Some(path);
        } else if let Some(file) = &mut spill {
            file.write_all(chunk).await?;
        } else {
            initial.extend_from_slice(chunk);
        }
    }

    if let Some(file) = &mut spill {
        file.flush().await?;
    }
    drop(spill);

    let total_lines = newline_count + u64::from(has_output && !ends_with_newline);
    let truncated = total_bytes > config.max_bytes as u64 || total_lines > config.max_lines as u64;
    let content = if truncated {
        render_tail(&tail, config)
    } else {
        String::from_utf8_lossy(&initial).into_owned()
    };
    Ok(CapturedStream {
        content,
        total_lines,
        total_bytes,
        truncated,
        full_output_path: full_output_path.map(|path| path.to_string_lossy().into_owned()),
    })
}

fn create_spill_file(scratch: &Path, label: &str) -> io::Result<(tokio::fs::File, PathBuf)> {
    let temporary = tempfile::Builder::new()
        .prefix(&format!("lam-{label}-"))
        .suffix(".log")
        .tempfile_in(scratch)?;
    let (file, path) = temporary.keep().map_err(|error| error.error)?;
    Ok((tokio::fs::File::from_std(file), path))
}

fn trim_rolling_tail(bytes: &mut Vec<u8>, maximum: usize) {
    if bytes.len() <= maximum || maximum == 0 {
        return;
    }
    let start = bytes.len() - maximum;
    bytes.drain(..start);
}

fn render_tail(bytes: &[u8], config: CaptureConfig) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    let mut lines = decoded.lines().collect::<Vec<_>>();
    if lines.len() > config.max_lines {
        lines.drain(..lines.len() - config.max_lines);
    }
    let joined = lines.join("\n");
    if joined.len() <= config.max_bytes {
        return joined;
    }
    let mut start = joined.len() - config.max_bytes;
    while start < joined.len() && !joined.is_char_boundary(start) {
        start += 1;
    }
    let suffix = &joined[start..];
    suffix.find('\n').map_or_else(
        || suffix.to_owned(),
        |newline| suffix[newline + 1..].to_owned(),
    )
}
