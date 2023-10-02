use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use docker_credential::DockerCredential;
use futures_util::future;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use oci_distribution::token_cache::RegistryTokenType;
use oci_distribution::RegistryOperation;
use oci_distribution::{
    client::{Config, ImageLayer},
    manifest::OciImageManifest,
    secrets::RegistryAuth,
    Reference,
};
use reqwest::Url;
use spin_app::locked::{ContentPath, ContentRef, LockedApp};
use spin_loader::cache::Cache;
use spin_manifest::Application;
use tokio::fs;
use walkdir::WalkDir;

use crate::auth::AuthConfig;

// TODO: the media types for application, wasm module, and data layer are not final.
const SPIN_APPLICATION_MEDIA_TYPE: &str = "application/vnd.fermyon.spin.application.v1+config";
const WASM_LAYER_MEDIA_TYPE: &str = "application/vnd.wasm.content.layer.v1+wasm";
const DATA_MEDIATYPE: &str = "application/vnd.wasm.content.layer.v1+data";

const CONFIG_FILE: &str = "config.json";
const LATEST_TAG: &str = "latest";
const MANIFEST_FILE: &str = "manifest.json";

const MAX_PARALLEL_PULL: usize = 16;

// Inline content into ContentRef iff < this size.
const CONTENT_REF_INLINE_MAX_SIZE: usize = 128;

/// Client for interacting with an OCI registry for Spin applications.
pub struct Client {
    /// Global cache for the metadata, Wasm modules, and static assets pulled from OCI registries.
    pub cache: Cache,
    /// Underlying OCI client.
    oci: oci_distribution::Client,
}

impl Client {
    /// Create a new instance of an OCI client for distributing Spin applications.
    pub async fn new(insecure: bool, cache_root: Option<PathBuf>) -> Result<Self> {
        let client = oci_distribution::Client::new(Self::build_config(insecure));
        let cache = Cache::new(cache_root).await?;

        Ok(Self { oci: client, cache })
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    pub async fn push(
        &mut self,
        app: &Application,
        reference: impl AsRef<str>,
    ) -> Result<Option<String>> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .with_context(|| format!("cannot parse reference {}", reference.as_ref()))?;
        let auth = Self::auth(&reference).await?;
        let working_dir = tempfile::tempdir()?;

        // Create a locked application from the application manifest.
        // TODO: We don't need an extra copy here for each asset to prepare the application.
        // We should be able to use assets::collect instead when constructing the locked app.
        let locked = spin_trigger::locked::build_locked_app(app.clone(), working_dir.path())
            .context("cannot create locked app")?;
        let locked = locked.clone();

        self.push_locked_core(locked, auth, reference).await
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    pub async fn push_locked(
        &mut self,
        locked: LockedApp,
        reference: impl AsRef<str>,
    ) -> Result<Option<String>> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .with_context(|| format!("cannot parse reference {}", reference.as_ref()))?;
        let auth = Self::auth(&reference).await?;

        self.push_locked_core(locked, auth, reference).await
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    async fn push_locked_core(
        &mut self,
        mut locked: LockedApp,
        auth: RegistryAuth,
        reference: Reference,
    ) -> Result<Option<String>> {
        // For each component in the application, add layers for the wasm module and
        // all static assets and update the locked application with the file digests.
        let mut layers = Vec::new();
        let mut components = Vec::new();

        for mut c in locked.components {
            // Add the wasm module for the component as layers.
            let source = c
                .clone()
                .source
                .content
                .source
                .context("component loaded from disk should contain a file source")?;

            let source = spin_trigger::parse_file_url(source.as_str())?;
            let layer = Self::wasm_layer(&source).await?;

            // Update the module source with the content ref of the layer.
            c.source.content = Self::content_ref_for_layer(&layer);

            layers.push(layer);

            // Add a layer for each file referenced in the mount directory.
            // Note that this is in fact a directory, and not a single file, so we need to
            // recursively traverse it and add layers for each file.
            let mut files = Vec::new();
            for f in c.files {
                let source = f
                    .content
                    .source
                    .context("file mount loaded from disk should contain a file source")?;
                let source = spin_trigger::parse_file_url(source.as_str())?;
                // Traverse each mount directory, add all static assets as layers, then update the
                // locked application file with the file digest.
                for entry in WalkDir::new(&source) {
                    let entry = entry?;
                    if entry.file_type().is_file() && !entry.file_type().is_dir() {
                        tracing::trace!(
                            "Adding new layer for asset {:?}",
                            spin_loader::to_relative(entry.path(), &source)?
                        );
                        let layer = Self::data_layer(entry.path()).await?;
                        let content = Self::content_ref_for_layer(&layer);
                        let content_inline = content.inline.is_some();
                        files.push(ContentPath {
                            content,
                            path: PathBuf::from(spin_loader::to_relative(entry.path(), &source)?),
                        });
                        // As a workaround for OCI implementations that don't support very small blobs,
                        // don't push very small content that has been inlined into the manifest:
                        // https://github.com/distribution/distribution/discussions/4029
                        let skip_layer = content_inline;
                        if !skip_layer {
                            layers.push(layer);
                        }
                    }
                }
            }
            c.files = files;
            components.push(c);
        }
        locked.components = components;
        locked.metadata.remove("origin");

        let oci_config = Config {
            data: serde_json::to_vec(&locked)?,
            media_type: SPIN_APPLICATION_MEDIA_TYPE.to_string(),
            annotations: None,
        };
        let manifest = OciImageManifest::build(&layers, &oci_config, None);
        let response = self
            .oci
            .push(&reference, &layers, oci_config, &auth, Some(manifest))
            .await
            .map(|push_response| push_response.manifest_url)
            .context("cannot push Spin application")?;

        tracing::info!("Pushed {:?}", response);

        let digest = digest_from_url(&response);
        Ok(digest)
    }

    /// Pull a Spin application from an OCI registry.
    pub async fn pull(&mut self, reference: &str) -> Result<()> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;
        let auth = Self::auth(&reference).await?;

        // Pull the manifest from the registry.
        let (manifest, digest) = self.oci.pull_image_manifest(&reference, &auth).await?;

        let manifest_json = serde_json::to_string(&manifest)?;
        tracing::debug!("Pulled manifest: {}", manifest_json);

        // Write the manifest in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/manifest.json`
        let m = self.manifest_path(&reference.to_string()).await?;
        fs::write(&m, &manifest_json).await?;

        let mut cfg_bytes = Vec::new();
        self.oci
            .pull_blob(&reference, &manifest.config.digest, &mut cfg_bytes)
            .await?;
        let cfg = std::str::from_utf8(&cfg_bytes)?;
        tracing::debug!("Pulled config: {}", cfg);

        // Write the config object in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/config.json`
        let c = self.lockfile_path(&reference.to_string()).await?;
        fs::write(&c, &cfg).await?;

        // If a layer is a Wasm module, write it in the Wasm directory.
        // Otherwise, write it in the data directory.
        stream::iter(manifest.layers)
            .map(|layer| {
                let this = &self;
                let reference = reference.clone();
                async move {
                    // Skip pulling if the digest already exists in the wasm or data directories.
                    if this.cache.wasm_file(&layer.digest).is_ok()
                        || this.cache.data_file(&layer.digest).is_ok()
                    {
                        tracing::debug!("Layer {} already exists in cache", &layer.digest);
                    } else {
                        tracing::debug!("Pulling layer {}", &layer.digest);
                        let mut bytes = Vec::new();
                        match this
                            .oci
                            .pull_blob(&reference, &layer.digest, &mut bytes)
                            .await
                        {
                            Err(e) => return Err(e),
                            _ => match layer.media_type.as_str() {
                                WASM_LAYER_MEDIA_TYPE => {
                                    let _ = this.cache.write_wasm(&bytes, &layer.digest).await;
                                }
                                _ => {
                                    let _ = this.cache.write_data(&bytes, &layer.digest).await;
                                }
                            },
                        }
                    }
                    Ok(())
                }
            })
            .buffer_unordered(MAX_PARALLEL_PULL)
            .try_for_each(future::ok)
            .await?;
        tracing::info!("Pulled {}@{}", reference, digest);

        Ok(())
    }

    /// Get the file path to an OCI manifest given a reference.
    /// If the directory for the manifest does not exist, this will create it.
    async fn manifest_path(&self, reference: impl AsRef<str>) -> Result<PathBuf> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .context("cannot parse OCI reference")?;
        let p = self
            .cache
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or(LATEST_TAG));

        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .context("cannot find directory for OCI manifest")?;
        }

        Ok(p.join(MANIFEST_FILE))
    }

    /// Get the file path to the OCI configuration object given a reference.
    pub async fn lockfile_path(&self, reference: impl AsRef<str>) -> Result<PathBuf> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .context("cannot parse reference")?;
        let p = self
            .cache
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or(LATEST_TAG));

        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .context("cannot find configuration object for reference")?;
        }

        Ok(p.join(CONFIG_FILE))
    }

    /// Create a new wasm layer based on a file.
    async fn wasm_layer(file: &Path) -> Result<ImageLayer> {
        tracing::log::trace!("Reading wasm module from {:?}", file);
        Ok(ImageLayer::new(
            fs::read(file).await.context("cannot read wasm module")?,
            WASM_LAYER_MEDIA_TYPE.to_string(),
            None,
        ))
    }

    /// Create a new data layer based on a file.
    async fn data_layer(file: &Path) -> Result<ImageLayer> {
        tracing::log::trace!("Reading data file from {:?}", file);
        Ok(ImageLayer::new(
            fs::read(&file).await?,
            DATA_MEDIATYPE.to_string(),
            None,
        ))
    }

    fn content_ref_for_layer(layer: &ImageLayer) -> ContentRef {
        ContentRef {
            // Inline small content as an optimization and to work around issues
            // with OCI implementations that don't support very small blobs.
            inline: (layer.data.len() <= CONTENT_REF_INLINE_MAX_SIZE).then(|| layer.data.to_vec()),
            digest: Some(layer.sha256_digest()),
            ..Default::default()
        }
    }

    /// Save a credential set containing the registry username and password.
    pub async fn login(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        // We want to allow a user to login to both https://ghcr.io and ghcr.io.
        let server = server.as_ref();
        let server = match server.parse::<Url>() {
            Ok(url) => url.host_str().unwrap_or(server).to_string(),
            Err(_) => server.to_string(),
        };

        // First, validate the credentials. If a user accidentally enters a wrong credential set, this
        // can catch the issue early rather than getting an error at the first operation that needs
        // to use the credentials (first time they do a push/pull/up).
        Self::validate_credentials(&server, &username, &password).await?;

        // Save an encoded representation of the credential set in the local configuration file.
        let mut auth = AuthConfig::load_default().await?;
        auth.insert(server, username, password)?;
        auth.save_default().await
    }

    /// Insert a token in the OCI client token cache.
    pub fn insert_token(
        &mut self,
        reference: &Reference,
        op: RegistryOperation,
        token: RegistryTokenType,
    ) {
        self.oci.tokens.insert(reference, op, token);
    }

    /// Validate the credentials by attempting to send an authenticated request to the registry.
    async fn validate_credentials(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        let client = dkregistry::v2::Client::configure()
            .registry(server.as_ref())
            .insecure_registry(false)
            .username(Some(username.as_ref().into()))
            .password(Some(password.as_ref().into()))
            .build()
            .context("cannot create client to send authentication request to the registry")?;

        match client
            // We don't need to configure any scopes, we are only testing that the credentials are
            // valid for the intended registry.
            .authenticate(&[""])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => bail!(format!(
                "cannot authenticate as {} to registry {}: {}",
                username.as_ref(),
                server.as_ref(),
                e
            )),
        }
    }

    /// Construct the registry authentication based on the reference.
    async fn auth(reference: &Reference) -> Result<RegistryAuth> {
        let server = reference
            .resolve_registry()
            .strip_suffix('/')
            .unwrap_or_else(|| reference.resolve_registry());

        match AuthConfig::get_auth_from_default(server).await {
            Ok(c) => Ok(c),
            Err(_) => match docker_credential::get_credential(server) {
                Err(e) => {
                    tracing::trace!("Cannot retrieve credentials from Docker, attempting to use anonymous auth: {}", e);
                    Ok(RegistryAuth::Anonymous)
                }

                Ok(DockerCredential::UsernamePassword(username, password)) => {
                    tracing::trace!("Found Docker credentials");
                    Ok(RegistryAuth::Basic(username, password))
                }
                Ok(DockerCredential::IdentityToken(_)) => {
                    tracing::trace!("Cannot use contents of Docker config, identity token not supported. Using anonymous auth");
                    Ok(RegistryAuth::Anonymous)
                }
            },
        }
    }

    /// Build the OCI client configuration given the insecure option.
    fn build_config(insecure: bool) -> oci_distribution::client::ClientConfig {
        let protocol = if insecure {
            oci_distribution::client::ClientProtocol::Http
        } else {
            oci_distribution::client::ClientProtocol::Https
        };

        oci_distribution::client::ClientConfig {
            protocol,
            ..Default::default()
        }
    }
}

fn digest_from_url(manifest_url: &str) -> Option<String> {
    // The URL is in the form "https://host/v2/refname/manifests/sha256:..."
    let manifest_url = Url::parse(manifest_url).ok()?;
    let segments = manifest_url.path_segments()?;
    let last = segments.last()?;
    if last.contains(':') {
        Some(last.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn can_parse_digest_from_manifest_url() {
        let manifest_url = "https://ghcr.io/v2/itowlson/osf/manifests/sha256:0a867093096e0ef01ef749b12b6e7a90e4952eda107f89a676eeedce63a8361f";
        let digest = digest_from_url(manifest_url).unwrap();
        assert_eq!(
            "sha256:0a867093096e0ef01ef749b12b6e7a90e4952eda107f89a676eeedce63a8361f",
            digest
        );
    }
}
