use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use super::models;

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ModelOption {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ModelCatalogSource {
    Fresh,
    Cache,
    StaleCache,
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelCatalog {
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

pub(super) fn load_openrouter_models() -> ModelCatalog {
    load_openrouter_models_with_cache_path(default_cache_path().ok().as_deref())
}

fn load_openrouter_models_with_cache_path(cache_path: Option<&Path>) -> ModelCatalog {
    if let Some(cache_path) = cache_path
        && let Ok(cache) = read_cache(cache_path)
        && !cache.models.is_empty()
        && cache_is_fresh(cache.fetched_at_unix)
    {
        return ModelCatalog {
            models: cache.models,
            source: ModelCatalogSource::Cache,
        };
    }

    match fetch_openrouter_models() {
        Ok(models) if !models.is_empty() => {
            if let Some(cache_path) = cache_path {
                let _ = write_cache(cache_path, &models);
            }

            ModelCatalog {
                models,
                source: ModelCatalogSource::Fresh,
            }
        }
        _ => {
            if let Some(cache_path) = cache_path
                && let Ok(cache) = read_cache(cache_path)
                && !cache.models.is_empty()
            {
                return ModelCatalog {
                    models: cache.models,
                    source: ModelCatalogSource::StaleCache,
                };
            }

            ModelCatalog {
                models: fallback_models(),
                source: ModelCatalogSource::Fallback,
            }
        }
    }
}

fn fetch_openrouter_models() -> io::Result<Vec<ModelOption>> {
    let response = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| io::Error::other(err.to_string()))?
        .get(OPENROUTER_MODELS_URL)
        .send()
        .map_err(|err| io::Error::other(err.to_string()))?
        .error_for_status()
        .map_err(|err| io::Error::other(err.to_string()))?
        .json::<OpenRouterModelsResponse>()
        .map_err(|err| io::Error::other(err.to_string()))?;

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

        let catalog = load_openrouter_models_with_cache_path(Some(&path));

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
