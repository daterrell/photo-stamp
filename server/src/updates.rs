//! Client updates, relayed from Forgejo's package registry: CI publishes every Windows build there
//! (`photo-stamp-windows`, version = the build's version). Devices ask this server for the newest
//! on their channel and download it from here with their device key; only the server needs a
//! token for the registry.
//!
//! Set `PHOTO_STAMP_UPDATES_TOKEN` (read access to packages) to turn this on; `PHOTO_STAMP_UPDATES_URL`
//! (default https://forgejo.terrell.ltd), `_OWNER` (dave) and `_PACKAGE` (photo-stamp-windows)
//! point it elsewhere.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use photo_stamp_proto::api::ClientUpdate;
use photo_stamp_proto::version::Version;
use serde::Deserialize;

/// Re-list the registry at most this often.
const LIST_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct UpdatesConfig {
    pub url: String,
    pub owner: String,
    pub package: String,
    pub token: String,
}

impl UpdatesConfig {
    /// From the environment; `None` if no token is set.
    pub fn from_env() -> Option<Self> {
        let var = |k: &str, d: &str| {
            std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_owned())
        };
        let token = std::env::var("PHOTO_STAMP_UPDATES_TOKEN").ok().filter(|t| !t.is_empty())?;
        Some(Self {
            url: var("PHOTO_STAMP_UPDATES_URL", "https://forgejo.terrell.ltd")
                .trim_end_matches('/')
                .to_owned(),
            owner: var("PHOTO_STAMP_UPDATES_OWNER", "dave"),
            package: var("PHOTO_STAMP_UPDATES_PACKAGE", "photo-stamp-windows"),
            token,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct PackageVersion {
    name: String,
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PackageFile {
    name: String,
    sha256: String,
}

/// A published build.
#[derive(Debug, Clone, PartialEq)]
pub struct Build {
    pub version: Version,
    pub file: String,
    pub sha256: String,
}

pub struct Updates {
    config: UpdatesConfig,
    agent: ureq::Agent,
    /// Builds by version, newest first, and when they were listed.
    listed: Mutex<Option<(Instant, Vec<Version>)>>,
    files: Mutex<std::collections::HashMap<String, (String, String)>>,
}

fn encode(v: &str) -> String {
    v.replace('+', "%2B")
}

impl Updates {
    pub fn new(config: UpdatesConfig) -> Self {
        Self {
            config,
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(10)))
                .user_agent(format!("photo-stamp-server/{}", photo_stamp_http::VERSION))
                .build()
                .into(),
            listed: Mutex::new(None),
            files: Mutex::new(Default::default()),
        }
    }

    fn get(&self, url: &str) -> anyhow::Result<ureq::http::Response<ureq::Body>> {
        let resp = self
            .agent
            .get(url)
            .header("Authorization", &format!("token {}", self.config.token))
            .call()
            .with_context(|| format!("reaching {}", self.config.url))?;
        match resp.status().as_u16() {
            200 => Ok(resp),
            401 | 403 => bail!("the package registry refused the token (PHOTO_STAMP_UPDATES_TOKEN)"),
            s => bail!("the package registry answered HTTP {s}"),
        }
    }

    /// Every published version, newest first.
    fn versions(&self) -> anyhow::Result<Vec<Version>> {
        if let Some((at, v)) = self.listed.lock().unwrap().as_ref()
            && at.elapsed() < LIST_EVERY
        {
            return Ok(v.clone());
        }
        let mut all = Vec::new();
        for page in 1..=20 {
            let url = format!(
                "{}/api/v1/packages/{}?type=generic&q={}&limit=50&page={page}",
                self.config.url, self.config.owner, self.config.package
            );
            let batch: Vec<PackageVersion> =
                serde_json::from_str(&self.get(&url)?.body_mut().read_to_string()?)?;
            let n = batch.len();
            all.extend(
                batch
                    .into_iter()
                    .filter(|p| p.name == self.config.package)
                    .filter_map(|p| Version::parse(&p.version)),
            );
            if n < 50 {
                break;
            }
        }
        all.sort_by(|a, b| b.cmp(a));
        *self.listed.lock().unwrap() = Some((Instant::now(), all.clone()));
        Ok(all)
    }

    /// The zip of a version: its file name and SHA-256.
    fn file(&self, version: &Version) -> anyhow::Result<(String, String)> {
        if let Some(f) = self.files.lock().unwrap().get(&version.text) {
            return Ok(f.clone());
        }
        let url = format!(
            "{}/api/v1/packages/{}/generic/{}/{}/files",
            self.config.url,
            self.config.owner,
            self.config.package,
            encode(&version.text)
        );
        let files: Vec<PackageFile> = serde_json::from_str(&self.get(&url)?.body_mut().read_to_string()?)?;
        let f = files.into_iter().find(|f| f.name.ends_with(".zip")).context("no zip in that build")?;
        let entry = (f.name, f.sha256);
        self.files.lock().unwrap().insert(version.text.clone(), entry.clone());
        Ok(entry)
    }

    /// The newest build on `channel` ("stable": releases only; "beta": anything).
    pub fn latest(&self, channel: &str) -> anyhow::Result<Option<ClientUpdate>> {
        let beta = channel == "beta";
        let Some(v) = self.versions()?.into_iter().find(|v| beta || v.is_stable()) else { return Ok(None) };
        let (_, sha256) = self.file(&v)?;
        Ok(Some(ClientUpdate {
            path: format!("/v1/client/download/{}", encode(&v.text)),
            version: v.text,
            sha256,
        }))
    }

    /// Open the zip of `version` for streaming (and its size, when known).
    pub fn download(&self, version: &str) -> anyhow::Result<(Option<u64>, Box<dyn Read + Send>)> {
        let v = Version::parse(version).context("not a version")?;
        if !self.versions()?.contains(&v) {
            bail!("no build {version}");
        }
        let (file, _) = self.file(&v)?;
        let url = format!(
            "{}/api/packages/{}/generic/{}/{}/{}",
            self.config.url,
            self.config.owner,
            self.config.package,
            encode(&v.text),
            encode(&file)
        );
        let resp = self.get(&url)?;
        let size =
            resp.headers().get("content-length").and_then(|h| h.to_str().ok()).and_then(|s| s.parse().ok());
        Ok((size, Box::new(resp.into_body().into_reader())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A stand-in package registry with three builds.
    fn registry() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            photo_stamp_http::serve(
                listener,
                Arc::new(|req: photo_stamp_http::Request| {
                    use photo_stamp_http::Response;
                    if req.header("authorization") != Some("token t") {
                        return Response::error(401, "no");
                    }
                    let p = req.path.as_str();
                    if p == "/api/v1/packages/dave" {
                        let list = serde_json::json!([
                            {"name": "photo-stamp-windows", "version": "0.7.0"},
                            {"name": "photo-stamp-windows", "version": "0.7.1-dev.3+gaaa"},
                            {"name": "photo-stamp-windows", "version": "0.6.0"},
                            {"name": "something-else", "version": "9.9.9"},
                        ]);
                        return Response::json(200, &list);
                    }
                    if let Some(rest) = p.strip_prefix("/api/v1/packages/dave/generic/photo-stamp-windows/") {
                        let v = rest.trim_end_matches("/files");
                        return Response::json(
                            200,
                            &serde_json::json!([{"name": format!("photo-stamp-windows-{v}.zip"), "sha256": format!("sha-of-{v}")}]),
                        );
                    }
                    if p.starts_with("/api/packages/dave/generic/photo-stamp-windows/") {
                        return Response::new(200, "application/zip", b"zipbytes".to_vec());
                    }
                    Response::error(404, "no")
                }),
            )
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn newest_per_channel_and_download() {
        let u = Updates::new(UpdatesConfig {
            url: registry(),
            owner: "dave".into(),
            package: "photo-stamp-windows".into(),
            token: "t".into(),
        });
        let stable = u.latest("stable").unwrap().unwrap();
        assert_eq!((stable.version.as_str(), stable.sha256.as_str()), ("0.7.0", "sha-of-0.7.0"));
        assert_eq!(stable.path, "/v1/client/download/0.7.0");
        let beta = u.latest("beta").unwrap().unwrap();
        assert_eq!(beta.version, "0.7.1-dev.3+gaaa");
        assert_eq!(beta.path, "/v1/client/download/0.7.1-dev.3%2Bgaaa");

        let (_, mut body) = u.download("0.7.1-dev.3+gaaa").unwrap();
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"zipbytes");
        assert!(u.download("9.9.9").is_err(), "only builds of this package");

        let bad = Updates::new(UpdatesConfig {
            url: registry(),
            owner: "dave".into(),
            package: "photo-stamp-windows".into(),
            token: "x".into(),
        });
        assert!(format!("{:#}", bad.latest("stable").unwrap_err()).contains("token"));
    }
}
