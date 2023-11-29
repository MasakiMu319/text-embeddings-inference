use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hf_hub::api::tokio::ApiBuilder;
use hf_hub::{Repo, RepoType};
use opentelemetry::sdk::propagation::TraceContextPropagator;
use opentelemetry::sdk::trace::Sampler;
use opentelemetry::sdk::{trace, Resource};
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use text_embeddings_backend::DType;
use text_embeddings_core::download::{download_artifacts, download_pool_config};
use text_embeddings_core::infer::Infer;
use text_embeddings_core::queue::Queue;
use text_embeddings_core::tokenization::Tokenization;
use text_embeddings_router::{ClassifierModel, EmbeddingModel, Info, ModelType};
use tokenizers::decoders::metaspace::PrependScheme;
use tokenizers::pre_tokenizers::sequence::Sequence;
use tokenizers::{PreTokenizerWrapper, Tokenizer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use veil::Redact;

/// App Configuration
#[derive(Parser, Redact)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The name of the model to load.
    /// Can be a MODEL_ID as listed on <https://hf.co/models> like
    /// `thenlper/gte-base`.
    /// Or it can be a local directory containing the necessary files
    /// as saved by `save_pretrained(...)` methods of transformers
    #[clap(default_value = "thenlper/gte-base", long, env)]
    #[redact(partial)]
    model_id: String,

    /// The actual revision of the model if you're referring to a model
    /// on the hub. You can use a specific commit id or a branch like `refs/pr/2`.
    #[clap(long, env)]
    revision: Option<String>,

    /// Optionally control the number of tokenizer workers used for payload tokenization, validation
    /// and truncation.
    /// Default to the number of CPU cores on the machine.
    #[clap(long, env)]
    tokenization_workers: Option<usize>,

    /// The dtype to be forced upon the model.
    #[clap(long, env, value_enum)]
    dtype: Option<DType>,

    /// Optionally control the pooling method for embedding models.
    ///
    /// If `pooling` is not set, the pooling configuration will be parsed from the
    /// model `1_Pooling/config.json` configuration.
    ///
    /// If `pooling` is set, it will override the model pooling configuration
    #[clap(long, env, value_enum)]
    pooling: Option<text_embeddings_backend::Pool>,

    /// The maximum amount of concurrent requests for this particular deployment.
    /// Having a low limit will refuse clients requests instead of having them
    /// wait for too long and is usually good to handle backpressure correctly.
    #[clap(default_value = "512", long, env)]
    max_concurrent_requests: usize,

    /// **IMPORTANT** This is one critical control to allow maximum usage
    /// of the available hardware.
    ///
    /// This represents the total amount of potential tokens within a batch.
    ///
    /// For `max_batch_tokens=1000`, you could fit `10` queries of `total_tokens=100`
    /// or a single query of `1000` tokens.
    ///
    /// Overall this number should be the largest possible until the model is compute bound.
    /// Since the actual memory overhead depends on the model implementation,
    /// text-embeddings-inference cannot infer this number automatically.
    #[clap(default_value = "16384", long, env)]
    max_batch_tokens: usize,

    /// Optionally control the maximum number of individual requests in a batch
    #[clap(long, env)]
    max_batch_requests: Option<usize>,

    /// Control the maximum number of inputs that a client can send in a single request
    #[clap(default_value = "32", long, env)]
    max_client_batch_size: usize,

    /// Your HuggingFace hub token
    #[clap(long, env)]
    #[redact(partial)]
    hf_api_token: Option<String>,

    /// The IP address to listen on
    #[clap(default_value = "0.0.0.0", long, env)]
    hostname: String,

    /// The port to listen on.
    #[clap(default_value = "3000", long, short, env)]
    port: u16,

    /// The name of the unix socket some text-embeddings-inference backends will use as they
    /// communicate internally with gRPC.
    #[clap(default_value = "/tmp/text-embeddings-inference-server", long, env)]
    uds_path: String,

    /// The location of the huggingface hub cache.
    /// Used to override the location if you want to provide a mounted disk for instance
    #[clap(long, env)]
    huggingface_hub_cache: Option<String>,

    /// Outputs the logs in JSON format (useful for telemetry)
    #[clap(long, env)]
    json_output: bool,

    #[clap(long, env)]
    otlp_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub architectures: Vec<String>,
    pub model_type: String,
    #[serde(alias = "n_positions")]
    pub max_position_embeddings: usize,
    pub pad_token_id: usize,
    pub id2label: Option<HashMap<String, String>>,
    pub label2id: Option<HashMap<String, usize>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PoolConfig {
    pooling_mode_cls_token: bool,
    pooling_mode_mean_tokens: bool,
    pooling_mode_max_tokens: bool,
    pooling_mode_mean_sqrt_len_tokens: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Pattern match configuration
    let args: Args = Args::parse();

    // Initialize loggin and telemetry
    init_logging(args.otlp_endpoint.clone(), args.json_output);

    tracing::info!("{args:?}");

    let model_id_path = Path::new(&args.model_id);
    let model_root = if model_id_path.exists() && model_id_path.is_dir() {
        // Using a local model
        model_id_path.to_path_buf()
    } else {
        let mut builder = ApiBuilder::new()
            .with_progress(false)
            .with_token(args.hf_api_token);

        if let Some(cache_dir) = args.huggingface_hub_cache {
            builder = builder.with_cache_dir(cache_dir.into());
        }

        let api = builder.build().unwrap();
        let api_repo = api.repo(Repo::with_revision(
            args.model_id.clone(),
            RepoType::Model,
            args.revision.clone().unwrap_or("main".to_string()),
        ));

        // Optionally download the pooling config.
        if args.pooling.is_none() {
            // If a pooling config exist, download it
            let _ = download_pool_config(&api_repo).await;
        }

        // Download model from the Hub
        download_artifacts(&api_repo)
            .await
            .context("Could not download model artifacts")?
    };

    // Load config
    let config_path = model_root.join("config.json");
    let config = fs::read_to_string(config_path).context("`config.json` not found")?;
    let config: ModelConfig =
        serde_json::from_str(&config).context("Failed to parse `config.json`")?;

    // Set model type from config
    let backend_model_type = {
        // Check if the model is a classifier
        let mut classifier = false;
        for arch in &config.architectures {
            if arch.ends_with("Classification") {
                classifier = true;
                break;
            }
        }

        if classifier {
            if args.pooling.is_some() {
                tracing::warn!(
                    "`--pooling` arg is set but model is a classifier. Ignoring `--pooling` arg."
                );
            }
            text_embeddings_backend::ModelType::Classifier
        } else {
            // Set pooling
            let pool = match args.pooling {
                Some(pool) => pool,
                None => {
                    // Load pooling config
                    let config_path = model_root.join("1_Pooling/config.json");
                    let config = fs::read_to_string(config_path).context("The `--pooling` arg is not set and we could not find a pooling configuration (`1_Pooling/config.json`) for this model.")?;
                    let config: PoolConfig = serde_json::from_str(&config)
                        .context("Failed to parse `1_Pooling/config.json`")?;
                    if config.pooling_mode_cls_token {
                        text_embeddings_backend::Pool::Cls
                    } else if config.pooling_mode_mean_tokens {
                        text_embeddings_backend::Pool::Mean
                    } else {
                        return Err(anyhow!("Pooling config {config:?} is not supported"));
                    }
                }
            };
            text_embeddings_backend::ModelType::Embedding(pool)
        }
    };

    // Info model type
    let model_type = match &backend_model_type {
        text_embeddings_backend::ModelType::Classifier => {
            let id2label = config
                .id2label
                .context("`config.json` does not contain `id2label`")?;
            let n_classes = id2label.len();
            let classifier_model = ClassifierModel {
                id2label,
                label2id: config
                    .label2id
                    .context("`config.json` does not contain `label2id`")?,
            };
            if n_classes > 1 {
                ModelType::Classifier(classifier_model)
            } else {
                ModelType::Reranker(classifier_model)
            }
        }
        text_embeddings_backend::ModelType::Embedding(pool) => {
            ModelType::Embedding(EmbeddingModel {
                pooling: pool.to_string(),
            })
        }
    };

    // Load tokenizer
    let tokenizer_path = model_root.join("tokenizer.json");
    let mut tokenizer = Tokenizer::from_file(tokenizer_path).expect(
        "tokenizer.json not found. text-embeddings-inference only supports fast tokenizers",
    );
    // See https://github.com/huggingface/tokenizers/pull/1357
    if let Some(pre_tokenizer) = tokenizer.get_pre_tokenizer() {
        if let PreTokenizerWrapper::Metaspace(m) = pre_tokenizer {
            // We are forced to clone since `Tokenizer` does not have a `get_mut` for `pre_tokenizer`
            let mut m = m.clone();
            m.set_prepend_scheme(PrependScheme::First);
            tokenizer.with_pre_tokenizer(PreTokenizerWrapper::Metaspace(m));
        } else if let PreTokenizerWrapper::Sequence(s) = pre_tokenizer {
            let pre_tokenizers = s.get_pre_tokenizers();
            // Check if we have a Metaspace pre tokenizer in the sequence
            let has_metaspace = pre_tokenizers
                .iter()
                .find(|t| matches!(t, PreTokenizerWrapper::Metaspace(_)))
                .is_some();

            if has_metaspace {
                let mut new_pre_tokenizers = Vec::with_capacity(s.get_pre_tokenizers().len());

                for pre_tokenizer in pre_tokenizers {
                    if let PreTokenizerWrapper::WhitespaceSplit(_) = pre_tokenizer {
                        // Remove WhitespaceSplit
                        // This will be done by the Metaspace pre tokenizer
                        continue;
                    }

                    let mut pre_tokenizer = pre_tokenizer.clone();

                    if let PreTokenizerWrapper::Metaspace(ref mut m) = pre_tokenizer {
                        m.set_prepend_scheme(PrependScheme::First);
                    }
                    new_pre_tokenizers.push(pre_tokenizer);
                }
                tokenizer.with_pre_tokenizer(PreTokenizerWrapper::Sequence(Sequence::new(
                    new_pre_tokenizers,
                )));
            }
        }
    }

    tokenizer.with_padding(None);

    // Position IDs offset. Used for Roberta and camembert.
    let position_offset = if &config.model_type == "xlm-roberta"
        || &config.model_type == "camembert"
        || &config.model_type == "roberta"
    {
        config.pad_token_id + 1
    } else {
        0
    };
    let max_input_length = config.max_position_embeddings - position_offset;

    let tokenization_workers = args
        .tokenization_workers
        .unwrap_or_else(num_cpus::get_physical);

    // Tokenization logic
    let tokenization = Tokenization::new(
        tokenization_workers,
        tokenizer,
        max_input_length,
        position_offset,
    );

    // Get dtype
    let dtype = args.dtype.unwrap_or({
        #[cfg(any(feature = "accelerate", feature = "mkl", feature = "mkl-dynamic"))]
        {
            DType::Float32
        }
        #[cfg(not(any(feature = "accelerate", feature = "mkl", feature = "mkl-dynamic")))]
        {
            DType::Float16
        }
    });

    // Create backend
    tracing::info!("Starting model backend");
    let backend = text_embeddings_backend::Backend::new(
        model_root,
        dtype.clone(),
        backend_model_type,
        args.uds_path,
        args.otlp_endpoint.clone(),
    )
    .context("Could not create backend")?;
    backend
        .health()
        .await
        .context("Model backend is not healthy")?;

    let max_batch_requests = backend
        .max_batch_size
        .map(|s| {
            tracing::warn!("Backend does not support a batch size > {s}");
            tracing::warn!("forcing `max_batch_requests={s}`");
            s
        })
        .or(args.max_batch_requests);

    // Queue logic
    let queue = Queue::new(
        backend.padded_model,
        args.max_batch_tokens,
        max_batch_requests,
        args.max_concurrent_requests,
    );

    // Create infer task
    let infer = Infer::new(tokenization, queue, args.max_concurrent_requests, backend);

    // Endpoint info
    let info = Info {
        model_id: args.model_id,
        model_sha: args.revision,
        model_dtype: dtype.to_string(),
        model_type,
        max_concurrent_requests: args.max_concurrent_requests,
        max_input_length,
        max_batch_tokens: args.max_batch_tokens,
        tokenization_workers,
        max_batch_requests,
        max_client_batch_size: args.max_client_batch_size,
        version: env!("CARGO_PKG_VERSION"),
        sha: option_env!("VERGEN_GIT_SHA"),
        docker_label: option_env!("DOCKER_LABEL"),
    };

    let addr = match args.hostname.parse() {
        Ok(ip) => SocketAddr::new(ip, args.port),
        Err(_) => {
            tracing::warn!("Invalid hostname, defaulting to 0.0.0.0");
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), args.port)
        }
    };

    tracing::info!("Ready");

    // Run axum server
    text_embeddings_router::run(infer, info, addr)
        .await
        .unwrap();

    if args.otlp_endpoint.is_some() {
        // Shutdown tracer
        global::shutdown_tracer_provider();
    }

    Ok(())
}

/// Init logging using env variables LOG_LEVEL and LOG_FORMAT:
///     - otlp_endpoint is an optional URL to an Open Telemetry collector
///     - LOG_LEVEL may be TRACE, DEBUG, INFO, WARN or ERROR (default to INFO)
///     - LOG_FORMAT may be TEXT or JSON (default to TEXT)
fn init_logging(otlp_endpoint: Option<String>, json_output: bool) {
    let mut layers = Vec::new();

    // STDOUT/STDERR layer
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_file(true)
        .with_line_number(true);

    let fmt_layer = match json_output {
        true => fmt_layer.json().flatten_event(true).boxed(),
        false => fmt_layer.boxed(),
    };
    layers.push(fmt_layer);

    // OpenTelemetry tracing layer
    if let Some(otlp_endpoint) = otlp_endpoint {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(otlp_endpoint),
            )
            .with_trace_config(
                trace::config()
                    .with_resource(Resource::new(vec![KeyValue::new(
                        "service.name",
                        "text-embeddings-inference.router",
                    )]))
                    .with_sampler(Sampler::AlwaysOn),
            )
            .install_batch(opentelemetry::runtime::Tokio);

        if let Ok(tracer) = tracer {
            layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
            init_tracing_opentelemetry::init_propagator().unwrap();
        };
    }

    // Filter events with LOG_LEVEL
    let env_filter =
        EnvFilter::try_from_env("LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layers)
        .init();
}
