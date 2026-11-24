use crate::{
    model::*,
    platform,
    snapshot::{self, Snapshot},
};
use anyhow::{Context, Result, bail};
use reqwest::{Client as HttpClient, Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const RECOVERY_WINDOW: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct HubStatus(StatusCode, String);

impl fmt::Display for HubStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hub returned {}: {}", self.0, self.1)
    }
}

impl std::error::Error for HubStatus {}

fn transient(error: &anyhow::Error) -> bool {
    let status = error.downcast_ref::<HubStatus>().map(|error| error.0);
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        if error.status().is_none() {
            return error.is_timeout() || error.is_connect() || error.is_request()
                || error.is_body() || error.is_decode();
        }
        return retryable_status(error.status());
    }
    retryable_status(status) || error.is::<tokio::time::error::Elapsed>()
}

fn retryable_status(status: Option<StatusCode>) -> bool {
    matches!(status, Some(StatusCode::REQUEST_TIMEOUT | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT))
}

#[derive(Default)]
struct Recovery {
    deadline: Option<Instant>,
    delay: Option<Duration>,
}

impl Recovery {
    async fn pause(&mut self) -> bool {
        let now = Instant::now();
        let deadline = *self.deadline.get_or_insert(now + RECOVERY_WINDOW);
        let delay = self.delay.unwrap_or(POLL_INTERVAL);
        self.delay = Some((delay * 2).min(MAX_BACKOFF));
        sleep_until((now + delay).min(deadline)).await;
        Instant::now() < deadline
    }

    async fn retry<T, F, Fut>(
        &mut self,
        mut operation: F,
        retryable: impl Fn(&anyhow::Error) -> bool,
    ) -> Result<T>
    where
        F: FnMut(Option<Instant>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        loop {
            match operation(self.deadline).await {
                Err(error) if retryable(&error) => {
                    if !self.pause().await {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }
}

async fn before_deadline<T>(
    deadline: Option<Instant>,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    match deadline {
        Some(deadline) => timeout_at(deadline, operation).await?,
        None => operation.await,
    }
}

fn uncertain_submission(error: &anyhow::Error) -> bool {
    transient(error) || error.is::<serde_json::Error>()
}

pub async fn resolve_latest_release_version() -> Result<String> {
    let url = std::env::var("YLCC_UPDATE_API_URL")
        .unwrap_or_else(|_| "https://api.github.com/repos/semanticist21/your-local-cloud-computing/releases/latest".into());
    let response = HttpClient::new()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("ylcc/{}", crate::VERSION))
        .timeout(Duration::from_secs(20))
        .send().await.context("failed to query GitHub releases")?
        .error_for_status().context("GitHub releases request failed")?;
    let body: serde_json::Value = response.json().await.context("invalid GitHub release response")?;
    let version = body["tag_name"].as_str().context("GitHub did not return tag_name")?;
    anyhow::ensure!(valid_release_version(version), "GitHub returned an invalid release tag");
    Ok(version.into())
}

pub fn read_token(path: &Path) -> Result<String> {
    let mut token = String::new();
    platform::read_private_file(path)?.read_to_string(&mut token)?;
    let token = token.trim().to_string();
    if token.len() < 32 || token.contains(char::is_whitespace) {
        bail!("token must contain at least 32 non-whitespace characters")
    };
    Ok(token)
}
pub fn validate_hub(hub: &str) -> Result<()> {
    let u = reqwest::Url::parse(hub)?;
    let local = u
        .host_str()
        .is_some_and(|h| ["localhost", "127.0.0.1", "[::1]", "::1"].contains(&h));
    if u.scheme() != "https" && !(u.scheme() == "http" && local) {
        bail!("hub must use HTTPS; HTTP is allowed only on loopback")
    }
    if !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("hub URL must not contain credentials, query or fragment")
    };
    Ok(())
}
pub struct Client {
    pub http: HttpClient,
    pub hub: String,
    token: String,
}
impl Client {
    pub fn new(hub: &str, token_file: &Path) -> Result<Self> {
        validate_hub(hub)?;
        Ok(Self {
            http: HttpClient::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .build()?,
            hub: hub.trim_end_matches('/').into(),
            token: read_token(token_file)?,
        })
    }
    pub fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.hub))
            .bearer_auth(&self.token)
    }
    async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(HubStatus(status, text).into());
        }
        Ok(serde_json::from_slice(&response.bytes().await?)?)
    }
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::decode(self.request(Method::GET, path).send().await?).await
    }
    pub async fn post<I: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        input: &I,
    ) -> Result<T> {
        Self::decode(self.request(Method::POST, path).json(input).send().await?).await
    }
    pub async fn put<I: Serialize, T: DeserializeOwned>(&self, path: &str, input: &I) -> Result<T> {
        Self::decode(self.request(Method::PUT, path).json(input).send().await?).await
    }
    pub async fn hub_capabilities(&self) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Health {
            #[serde(default)]
            capabilities: Option<Vec<String>>,
        }
        let health: Health = Self::decode(self.request(Method::GET, "/health")
            .timeout(STATUS_TIMEOUT).send().await?).await?;
        Ok(health.capabilities.unwrap_or_default())
    }

    pub async fn submit_job(&self, spec: &JobSpec, idempotent: bool) -> Result<Job> {
        if !idempotent {
            return match self.send_job("/v1/jobs", spec, None).await {
                Err(error) if uncertain_submission(&error) => {
                    Err(error.context("submission outcome unconfirmed; check ylcc jobs list before resubmitting"))
                }
                result => result,
            };
        }

        let key = Uuid::new_v4().to_string();
        let mut recovery = Recovery::default();
        let result = recovery.retry(
            |deadline| before_deadline(deadline, self.send_job(IDEMPOTENT_JOBS_PATH, spec, Some(&key))),
            uncertain_submission,
        ).await;
        match result {
            Err(error) if recovery.deadline.is_some()
                && !error.downcast_ref::<HubStatus>().is_some_and(|status|
                    matches!(status.0, StatusCode::CONFLICT | StatusCode::GONE)) =>
            {
                Err(error.context("submission outcome unconfirmed; check ylcc jobs list before resubmitting"))
            }
            result => result,
        }
    }

    async fn send_job(&self, route: &str, spec: &JobSpec, key: Option<&str>) -> Result<Job> {
        let mut request = self.request(Method::POST, route).timeout(RECOVERY_WINDOW);
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        Self::decode(request.json(spec).send().await?).await
    }

    pub async fn upload(&self, s: &Snapshot) -> Result<(usize, u64)> {
        let hashes = s.files.iter().map(|f| f.hash.clone()).collect();
        let missing: MissingBlobs = self
            .post("/v1/blobs/missing", &MissingBlobs { hashes })
            .await?;
        let mut bytes = 0;
        for hash in &missing.hashes {
            validate_hash(hash)?;
            let path = s.blobs.path().join(hash);
            let file = tokio::fs::File::open(&path)
                .await
                .context("hub requested blob outside snapshot")?;
            let len = file.metadata().await?.len();
            let r = self
                .request(Method::PUT, &format!("/v1/blobs/{hash}"))
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .send()
                .await?;
            if !r.status().is_success() {
                bail!("blob upload rejected: {}", r.status())
            };
            bytes += len;
        }
        Ok((missing.hashes.len(), bytes))
    }

    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::decode(self.request(Method::DELETE, path).send().await?).await
    }
    pub async fn delete(&self, path: &str) -> Result<()> {
        let response = self
            .http
            .delete(format!("{}{}", self.hub, path))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("HTTP {}: {}: {}", status, path, text)
        }
        Ok(())
    }
    pub async fn job(&self, id: &str) -> Result<Job> {
        segment(id)?;
        self.get(&format!("/v1/jobs/{id}")).await
    }
    pub async fn wait(&self, id: &str, results_root: &Path, download: bool) -> Result<Job> {
        segment(id)?;
        let route = format!("/v1/jobs/{id}");
        let mut recovery = Recovery::default();
        let mut last_state = None;
        loop {
            let observation = before_deadline(recovery.deadline, async {
                Self::decode::<Job>(self.request(Method::GET, &route)
                    .timeout(STATUS_TIMEOUT).send().await?).await
            }).await;
            let detail = match observation {
                Ok(job) => {
                    last_state = Some(job.state.clone());
                    if job_is_terminal(&job.state) {
                        if download {
                            self.fetch(&job, results_root).await?;
                        }
                        return Ok(job);
                    }
                    if job.state != "unknown" {
                        recovery = Recovery::default();
                        sleep(POLL_INTERVAL).await;
                        continue;
                    }
                    job.error.unwrap_or_else(|| "worker execution state is unknown".into())
                }
                Err(error) if transient(&error) => error.to_string(),
                Err(error) => return Err(error.context(format!("cannot check job {id}"))),
            };
            if recovery.deadline.is_none() {
                eprintln!("job {id}: {detail}; checking again for up to 60 seconds");
            }
            if !recovery.pause().await {
                let state = last_state.as_deref().unwrap_or("unavailable");
                let flags = if download { "" } else { " --no-download" };
                bail!("could not confirm job {id} within 60 seconds (last state: {state}): {detail}; resume with ylcc jobs wait {id}{flags}");
            }
        }
    }
    pub async fn fetch(&self, job: &Job, results_root: &Path) -> Result<PathBuf> {
        segment(&job.id)?;
        // The explicitly selected root may use OS aliases such as macOS /var -> /private/var.
        fs::create_dir_all(results_root)?;
        let results_root = platform::command_path(&results_root.canonicalize()?);
        let dest = results_root.join(&job.id);
        snapshot::ensure_safe_directory(&dest)?;
        for f in &job.results {
            platform::filesystem_relative(&f.path)?;
            validate_hash(&f.hash)?;
            let target = dest.join(&f.path);
            snapshot::ensure_safe_directory(target.parent().context("result has no parent")?)?;
            if let Ok(meta) = fs::symlink_metadata(&target) {
                if !meta.is_file() || platform::is_link(&meta) {
                    bail!("refusing non-regular result destination")
                };
                if snapshot::hash_file(&target)? == f.hash {
                    continue;
                }
                bail!(
                    "result destination has different content; refusing overwrite: {}",
                    target.display()
                )
            }
            Recovery::default()
                .retry(|deadline| self.fetch_file(f, &target, deadline), transient).await
                .with_context(|| format!("fetch result {} for job {}; resume with ylcc jobs fetch {}", f.path, job.id, job.id))?;
        }
        Ok(dest)
    }

    async fn fetch_file(&self, file: &FileEntry, target: &Path, deadline: Option<Instant>) -> Result<()> {
        let response = before_deadline(deadline, async {
            Ok(timeout(STATUS_TIMEOUT, self
                .request(Method::GET, &format!("/v1/blobs/{}", file.hash))
                .send()).await??.error_for_status()?)
        }).await?;
        let stream = response.bytes_stream();
        let mut tmp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        let mut hash = Sha256::new();
        let mut size = 0;
        use futures_util::StreamExt;
        futures_util::pin_mut!(stream);
        while let Some(chunk) = timeout(STATUS_TIMEOUT, stream.next()).await? {
            let bytes = chunk?;
            size += bytes.len() as u64;
            if size > file.size {
                bail!("result exceeds declared size")
            }
            hash.update(&bytes);
            tmp.write_all(&bytes)?;
        }
        if size != file.size || hex::encode(hash.finalize()) != file.hash {
            bail!("result checksum/size mismatch")
        }
        platform::set_mode(tmp.as_file(), file.mode)?;
        tmp.persist_noclobber(target)?;
        Ok(())
    }
}
pub fn segment(s: &str) -> Result<()> {
    safe_relative(s)?;
    if !s
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        bail!("expected one path segment")
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wait_recovers_status_and_download_for_the_same_job() {
        use axum::{Json, Router, body::{Body, Bytes}, response::IntoResponse, routing::get};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

        let status_count = Arc::new(AtomicUsize::new(0));
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let hash = hex::encode(Sha256::digest(b"complete"));
        let app = Router::new()
            .route("/v1/jobs/example", get({
                let status_count = status_count.clone();
                let hash = hash.clone();
                move || {
                    let sequence = status_count.fetch_add(1, Ordering::SeqCst);
                    let hash = hash.clone();
                    async move {
                        if sequence == 0 {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        Json(serde_json::json!({
                            "id": "example", "attempt_id": "original-attempt",
                            "state": if sequence == 1 { "unknown" } else { "succeeded" },
                            "spec": {"command": ["build"]}, "created_at": 1, "updated_at": 2,
                            "results": [{"path": "artifact", "hash": hash, "size": 8, "mode": 420}]
                        })).into_response()
                    }
                }
            }))
            .route(&format!("/v1/blobs/{hash}"), get({
                let fetch_count = fetch_count.clone();
                move || {
                    let sequence = fetch_count.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if sequence == 0 {
                            let interrupted = futures_util::stream::iter([
                                Ok(Bytes::from_static(b"comp")),
                                Err(std::io::Error::other("transfer interrupted")),
                            ]);
                            return Body::from_stream(interrupted).into_response();
                        }
                        "complete".into_response()
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client { http: HttpClient::builder().no_proxy().build().unwrap(), hub: address, token: String::new() };
        let results = tempfile::tempdir().unwrap();
        let job = client.wait("example", results.path(), true).await.unwrap();
        assert_eq!(job.attempt_id, "original-attempt");
        assert_eq!(status_count.load(Ordering::SeqCst), 3);
        assert_eq!(fetch_count.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read(results.path().join("example/artifact")).unwrap(), b"complete");
        client.fetch(&job, results.path()).await.unwrap();
        assert_eq!(fetch_count.load(Ordering::SeqCst), 2, "verified files are reused");
        server.abort();
    }

    #[tokio::test]
    async fn recovery_stops_at_its_deadline_and_does_not_retry_permanent_errors() {
        let mut recovery = Recovery { deadline: Some(Instant::now()), delay: None };
        assert!(!recovery.pause().await);
        for status in [StatusCode::UNAUTHORIZED, StatusCode::NOT_FOUND, StatusCode::UNPROCESSABLE_ENTITY] {
            assert!(!transient(&HubStatus(status, String::new()).into()));
        }
        assert!(!transient(&anyhow::anyhow!("result checksum/size mismatch")));
    }

    #[test]
    fn no_cleartext_remote_or_embedded_credentials() {
        assert!(validate_hub("http://127.0.0.1:7443").is_ok());
        assert!(validate_hub("https://example.test:10000").is_ok());
        assert!(validate_hub("http://example.test").is_err());
        assert!(validate_hub("https://a:b@example.test").is_err());
    }
    #[test]
    fn route_segments_cannot_change_the_url() {
        for name in [
            "job?other",
            "job#fragment",
            "job/other",
            "job:other",
            "../job",
        ] {
            assert!(segment(name).is_err());
        }
        assert!(segment("job-1_a.b").is_ok());
    }
}
