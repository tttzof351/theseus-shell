use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::common::cancellation::CancellationEvent;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::models;

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelOption {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelCatalogSource {
    Fresh,
    Cache,
    StaleCache,
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCatalog {
    pub models: Vec<ModelOption>,
    pub source: ModelCatalogSource,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelsResponse {
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    name: Option<String>,
    context_length: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedModels {
    fetched_at_unix: u64,
    models: Vec<ModelOption>,
}

pub(crate) fn load_openrouter_models(cancellation: &CancellationEvent) -> io::Result<ModelCatalog> {
    load_openrouter_models_with_cache_path(
        default_cache_path().ok().as_deref(),
        OPENROUTER_MODELS_URL,
        cancellation,
    )
}

fn load_openrouter_models_with_cache_path(
    cache_path: Option<&Path>,
    url: &str,
    cancellation: &CancellationEvent,
) -> io::Result<ModelCatalog> {
    if cancellation.is_cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if let Some(cache_path) = cache_path
        && let Ok(cache) = read_cache(cache_path)
        && !cache.models.is_empty()
        && cache_is_fresh(cache.fetched_at_unix)
    {
        return Ok(ModelCatalog {
            models: cache.models,
            source: ModelCatalogSource::Cache,
        });
    }

    match fetch_openrouter_models(url, cancellation) {
        Ok(models) if !models.is_empty() => {
            if cancellation.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if let Some(cache_path) = cache_path {
                let _ = write_cache(cache_path, &models);
            }

            Ok(ModelCatalog {
                models,
                source: ModelCatalogSource::Fresh,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
        _ => {
            if let Some(cache_path) = cache_path
                && let Ok(cache) = read_cache(cache_path)
                && !cache.models.is_empty()
            {
                return Ok(ModelCatalog {
                    models: cache.models,
                    source: ModelCatalogSource::StaleCache,
                });
            }

            Ok(ModelCatalog {
                models: fallback_models(),
                source: ModelCatalogSource::Fallback,
            })
        }
    }
}

fn fetch_openrouter_models(
    url: &str,
    cancellation: &CancellationEvent,
) -> io::Result<Vec<ModelOption>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let response: OpenRouterModelsResponse = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(io::Error::from(io::ErrorKind::Interrupted)),
            result = async {
                Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))?
        .get(url)
        .send().await
        .map_err(|err| io::Error::other(err.to_string()))?
        .error_for_status()
        .map_err(|err| io::Error::other(err.to_string()))?
        .json::<OpenRouterModelsResponse>().await
        .map_err(|err| io::Error::other(err.to_string()))
            } => result,
        }
    })?;

    let mut models = response
        .data
        .into_iter()
        .filter(|model| !model.id.trim().is_empty())
        .map(|model| ModelOption {
            id: model.id,
            name: model.name.filter(|name| !name.trim().is_empty()),
            context_length: model.context_length,
        })
        .collect::<Vec<_>>();

    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);

    Ok(models)
}

fn fallback_models() -> Vec<ModelOption> {
    models::AVAILABLE_MODELS
        .iter()
        .map(|model| ModelOption {
            id: (*model).to_string(),
            name: None,
            context_length: None,
        })
        .collect()
}

fn read_cache(path: &Path) -> io::Result<CachedModels> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn write_cache(path: &Path, models: &[ModelOption]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let cache = CachedModels {
        fetched_at_unix: current_unix_timestamp(),
        models: models.to_vec(),
    };
    let text =
        serde_json::to_string_pretty(&cache).map_err(|err| io::Error::other(err.to_string()))?;

    fs::write(path, text)
}

fn cache_is_fresh(fetched_at_unix: u64) -> bool {
    current_unix_timestamp().saturating_sub(fetched_at_unix) <= CACHE_TTL.as_secs()
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_cache_path() -> io::Result<PathBuf> {
    home_dir()
        .map(|home| {
            home.join(".theseus")
                .join("persist")
                .join("openrouter_models.json")
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_fetch_cancel_drops_a_stalled_response_instead_of_using_fallback() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::mpsc,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let (ready_tx, ready) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "catalog request never arrived"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 1024];
            while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                let count = stream.read(&mut bytes).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10000\r\n\r\n{\"data\":[").unwrap();
            ready_tx.send(()).unwrap();
            match stream.read(&mut bytes) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) => {}
                result => panic!("cancel left the catalog response alive: {result:?}"),
            }
        });
        let cancellation = CancellationEvent::new();
        let worker_cancel = cancellation.clone();
        let worker = thread::spawn(move || {
            load_openrouter_models_with_cache_path(None, &url, &worker_cancel)
        });
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        cancellation.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        server.join().unwrap();
    }

    #[test]
    fn uses_fresh_cache_without_network() {
        let path = env::temp_dir().join(format!(
            "theseus-openrouter-models-cache-fresh-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);

        let cache = CachedModels {
            fetched_at_unix: current_unix_timestamp(),
            models: vec![ModelOption {
                id: "cached/model".to_string(),
                name: Some("Cached Model".to_string()),
                context_length: Some(128_000),
            }],
        };
        fs::write(&path, serde_json::to_string(&cache).unwrap()).unwrap();

        let catalog = load_openrouter_models_with_cache_path(
            Some(&path),
            OPENROUTER_MODELS_URL,
            &CancellationEvent::new(),
        )
        .unwrap();

        assert_eq!(catalog.source, ModelCatalogSource::Cache);
        assert_eq!(catalog.models[0].id, "cached/model");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn static_fallback_models_include_default_model() {
        let models = fallback_models();

        assert!(!models.is_empty());
        assert!(
            models
                .iter()
                .any(|model| model.id == super::models::DEFAULT_MODEL)
        );
    }
}
