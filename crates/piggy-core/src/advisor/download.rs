//! Fetching and verifying advisor weights.
//!
//! This downloads **data, never an executable**. That is the whole reason the
//! advisor links llama.cpp into this binary instead of fetching a `llama-server`
//! release: a downloaded binary lands unsigned on disk and has to be argued past
//! Gatekeeper, whereas a `.gguf` is inert until our own signed code opens it.
//!
//! Three things make the file trustworthy, in order of how much they matter:
//!
//! 1. **A pinned sha256.** [`super::CATALOG`] carries the digest, authored from
//!    the Hugging Face API ahead of time. A digest fetched at download time from
//!    the same host that served the bytes would verify nothing.
//! 2. **A pinned length**, checked first, so a truncated or padded transfer is
//!    rejected before we spend seconds hashing gigabytes.
//! 3. **A host allow-list on redirects**, so a hijacked redirect cannot make us
//!    pull from an arbitrary origin. Defence in depth: the digest is the control
//!    that actually decides, and this only narrows what we will talk to.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::AdvisorModel;

/// Read/write chunk. Large enough that progress callbacks do not dominate the
/// transfer, small enough that cancellation feels immediate.
const CHUNK: usize = 1024 * 1024;

/// Consecutive attempts that make **no progress** before giving up.
///
/// A connection dropping partway through a gigabyte is ordinary, not
/// exceptional: the first live run of this code died at 63% with "end of file
/// before message length reached". Since the partial file is a valid resume
/// point, the right response is to carry on from it rather than hand the user a
/// failure and a button to press again. Only attempts that move zero bytes count
/// toward this limit, so a flaky link that keeps inching forward is never
/// abandoned.
const MAX_STALLED_ATTEMPTS: usize = 6;

/// Hosts we will follow a redirect to.
///
/// Hugging Face resolves `/resolve/main/...` to its Xet CDN, currently
/// `us.aws.cdn.hf.co`, so both apex domains and their subdomains are in scope.
fn is_hf_host(host: &str) -> bool {
    host == "huggingface.co"
        || host.ends_with(".huggingface.co")
        || host == "hf.co"
        || host.ends_with(".hf.co")
}

fn client() -> Result<reqwest::blocking::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let ok = attempt.url().host_str().map(is_hf_host).unwrap_or(false);
        if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else if ok {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    Ok(reqwest::blocking::Client::builder()
        .redirect(policy)
        .user_agent("piggy/0.1")
        .connect_timeout(std::time::Duration::from_secs(30))
        // Blocking reqwest defaults to a 30-second timeout for the WHOLE
        // request, body included, which would abort every download here well
        // short of a gigabyte. Disabling it is mandatory, not a tuning choice.
        // A stalled socket is instead bounded by the caller's cancel flag.
        .timeout(None)
        .build()?)
}

/// The origin URL for a model's weights.
///
/// Resume always re-resolves through this, never through a cached CDN link:
/// the signed URL Hugging Face hands out embeds a `ByteRange` condition, so a
/// link obtained for one range is not valid for the next.
fn url(m: &AdvisorModel) -> String {
    format!(
        "https://huggingface.co/{}/resolve/main/{}",
        m.repo, m.file
    )
}

fn part_path(m: &AdvisorModel) -> PathBuf {
    m.path().with_extension("gguf.part")
}

/// Download `m` into [`AdvisorModel::path`], resuming a previous attempt.
///
/// `progress` is called with `(received, total)` roughly once per megabyte.
/// Setting `cancel` stops the transfer and leaves the partial file in place, so
/// the next call resumes rather than restarting a multi-gigabyte download.
///
/// On success the file at [`AdvisorModel::path`] is length- and digest-verified.
/// On digest failure the partial file is **deleted**, not kept: a resumable
/// download that resumes onto corrupt bytes can never converge.
pub fn fetch(
    m: &AdvisorModel,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let dest = m.path();
    if dest.exists() && verify(m).is_ok() {
        progress(m.bytes, m.bytes);
        return Ok(());
    }

    std::fs::create_dir_all(super::models_dir()).context("creating the models directory")?;
    let part = part_path(m);

    // A longer partial than the finished file means it is not this file.
    if std::fs::metadata(&part).map(|md| md.len()).unwrap_or(0) > m.bytes {
        std::fs::remove_file(&part).ok();
    }

    let mut stalled = 0usize;
    loop {
        let have = std::fs::metadata(&part).map(|md| md.len()).unwrap_or(0);
        if have >= m.bytes {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            bail!("download cancelled");
        }

        match stream_from(m, &part, have, cancel, &mut progress) {
            Ok(()) => {}
            Err(e) => {
                // Cancellation is a decision, not a fault. Retrying it would
                // ignore the user.
                if cancel.load(Ordering::Relaxed) {
                    return Err(e);
                }
                let now = std::fs::metadata(&part).map(|md| md.len()).unwrap_or(0);
                // Any forward progress means the link is usable and the failure
                // was transient, so the budget resets.
                stalled = if now > have { 0 } else { stalled + 1 };
                if stalled >= MAX_STALLED_ATTEMPTS {
                    return Err(e).with_context(|| {
                        format!(
                            "gave up after {MAX_STALLED_ATTEMPTS} attempts with no progress \
                             ({now} of {} bytes downloaded; the partial file was kept, so \
                             starting again will resume)",
                            m.bytes
                        )
                    });
                }
                // Linear backoff, in cancellable slices so Cancel stays instant.
                for _ in 0..(stalled * 4) {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("download cancelled");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }

    // Length before digest: catches the common failure (a short transfer) in a
    // stat instead of by hashing gigabytes.
    let got = std::fs::metadata(&part)?.len();
    if got != m.bytes {
        bail!(
            "downloaded {got} bytes but {} should be {} bytes; the transfer was incomplete",
            m.file,
            m.bytes
        );
    }

    let actual = sha256_file(&part)?;
    if actual != m.sha256 {
        // Corrupt bytes must not survive as a resume point.
        std::fs::remove_file(&part).ok();
        bail!(
            "{} failed verification (expected sha256 {}, got {}); the file was discarded",
            m.file,
            &m.sha256[..16],
            &actual[..16]
        );
    }

    std::fs::rename(&part, &dest)
        .with_context(|| format!("moving verified weights into {}", dest.display()))?;
    Ok(())
}

/// One transfer attempt, appending to `part` from byte `have`.
///
/// Returns `Err` on a dropped connection, which the caller treats as resumable
/// rather than fatal. Whatever bytes made it to disk stay there and become the
/// next attempt's offset, so a failure here is progress, not waste.
fn stream_from(
    m: &AdvisorModel,
    part: &Path,
    have: u64,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    let client = client()?;
    let mut req = client.get(url(m));
    if have > 0 {
        // Re-resolved through the origin every time. The signed URL Hugging Face
        // hands out embeds a `ByteRange` condition, so a link obtained for one
        // range is not valid for the next.
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = req
        .send()
        .with_context(|| format!("GET {}", url(m)))?
        .error_for_status()
        .context("the weights host returned an error status")?;

    // A server that ignores our Range header sends 200 with the whole file.
    // Appending that to a partial would silently produce a corrupt blob that
    // only fails at the digest, after another full download.
    let resuming = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut received = if resuming { have } else { 0 };

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!resuming)
        .append(resuming)
        .open(part)
        .with_context(|| format!("opening {}", part.display()))?;

    let mut body = resp;
    let mut buf = vec![0u8; CHUNK];
    progress(received, m.bytes);
    loop {
        if cancel.load(Ordering::Relaxed) {
            file.flush().ok();
            bail!("download cancelled");
        }
        let n = body.read(&mut buf).context("reading from the weights host")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", part.display()))?;
        received += n as u64;
        progress(received, m.bytes);
    }
    file.flush().context("flushing the downloaded weights")?;
    Ok(())
}

/// Re-verify weights already on disk: length, then digest.
///
/// The UI does not call this on every poll (see [`AdvisorModel::present`]); it
/// runs before the first load of a session, so a file corrupted on disk since
/// download fails here rather than inside llama.cpp.
pub fn verify(m: &AdvisorModel) -> Result<()> {
    let path = m.path();
    let got = std::fs::metadata(&path)
        .with_context(|| format!("{} is not downloaded", m.file))?
        .len();
    if got != m.bytes {
        bail!("{} is {got} bytes, expected {}", m.file, m.bytes);
    }
    let actual = sha256_file(&path)?;
    if actual != m.sha256 {
        bail!("{} failed its sha256 check", m.file);
    }
    Ok(())
}

/// Delete a model's weights and any partial download.
pub fn remove(m: &AdvisorModel) -> Result<()> {
    for p in [m.path(), part_path(m)] {
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    Ok(())
}

/// Streaming sha256 so a 2.5 GB file is never held in memory.
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} to verify it", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
