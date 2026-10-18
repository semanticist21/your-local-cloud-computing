use crate::{
    model::*,
    platform,
    snapshot::{self, Snapshot},
};
use anyhow::{Context, Result, bail};
use reqwest::{Client as HttpClient, Method};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::io::ReaderStream;

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
            let text = response.text().await?;
            bail!("hub returned {status}: {text}")
        };
        Ok(response.json().await?)
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
        loop {
            let j = self.job(id).await?;
            match j.state.as_str() {
                "succeeded" | "failed" | "cancelled" => {
                    if download {
                        self.fetch(&j, results_root).await?;
                    }
                    return Ok(j);
                }
                "unknown" => bail!(
                    "job {id} has unknown execution state; inspect/reconnect its worker before retrying"
                ),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
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
            let response = self
                .request(Method::GET, &format!("/v1/blobs/{}", f.hash))
                .send()
                .await?
                .error_for_status()?;
            let stream = response.bytes_stream();
            let mut tmp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
            let mut hash = Sha256::new();
            let mut size = 0;
            use futures_util::StreamExt;
            futures_util::pin_mut!(stream);
            while let Some(chunk) = stream.next().await {
                let b = chunk?;
                size += b.len() as u64;
                if size > f.size {
                    bail!("result exceeds declared size")
                };
                hash.update(&b);
                tmp.write_all(&b)?;
            }
            if size != f.size || hex::encode(hash.finalize()) != f.hash {
                bail!("result checksum/size mismatch")
            }
            platform::set_mode(tmp.as_file(), f.mode)?;
            tmp.persist_noclobber(&target)?;
        }
        Ok(dest)
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
