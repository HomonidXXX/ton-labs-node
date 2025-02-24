mod block;
mod block_proof;
mod boot;
mod collator_test_bundle;
pub mod config;
mod engine;
mod engine_traits;
mod engine_operations;
mod error;
mod full_node;
mod internal_db;
mod macros;
pub mod network;
mod rng;
mod shard_blocks;
mod shard_state;
mod sync;
mod types;
mod validating_utils;
mod validator;

#[cfg(feature = "tracing")]
mod jaeger;

#[cfg(not(feature = "tracing"))]
mod jaeger {
    pub fn init_jaeger(){}
    #[cfg(feature = "external_db")]
    pub fn message_from_kafka_received(_kf_key: &[u8]) {}
    pub fn broadcast_sended(_msg_id: String) {}
}

//extern crate lazy_static;

#[cfg(feature = "external_db")]
mod external_db;
mod ext_messages;

use crate::{config::TonNodeConfig, engine_traits::ExternalDb, engine::STATSD, jaeger::init_jaeger};
use clap;

#[cfg(feature = "external_db")]
use ton_types::error;
use ton_types::Result;
use std::sync::Arc;


fn init_logger(log_config_path: Option<String>) {

    if let Some(path) = log_config_path {
        if let Err(err) = log4rs::init_file(path, Default::default()) {
            println!("Error while initializing log by {}: {}", err, err);
        } else {
            return;
        }
    }

    let level = log::LevelFilter::Info; 
    let stdout = log4rs::append::console::ConsoleAppender::builder()
        .target(log4rs::append::console::Target::Stdout)
        .build();

    let config = log4rs::config::Config::builder()
        .appender(
            log4rs::config::Appender::builder()
                .filter(Box::new(log4rs::filter::threshold::ThresholdFilter::new(level)))
                .build("stdout", Box::new(stdout)),
        )
        .build(
            log4rs::config::Root::builder()
                .appender("stdout")
                .build(log::LevelFilter::Info),
        )
        .unwrap();

    let result = log4rs::init_config(config);
    if let Err(e) = result {
        println!("Error init log: {}", e);
    }
}

fn log_version() {
    log::info!(
        "Execute {:?}\nCOMMIT_ID: {:?}\nBUILD_DATE: {:?}\nCOMMIT_DATE: {:?}\nGIT_BRANCH: {:?}\n", // RUST_VERSION:{}\n
        std::option_env!("CARGO_PKG_VERSION").unwrap_or("Not set"),
        std::option_env!("BUILD_GIT_COMMIT").unwrap_or("Not set"),
        std::option_env!("BUILD_TIME").unwrap_or("Not set"),
        std::option_env!("BUILD_GIT_DATE").unwrap_or("Not set"),
        std::option_env!("BUILD_GIT_BRANCH").unwrap_or("Not set"),
        //std::env!("BUILD_RUST_VERSION") // TODO
    );
}

fn print_build_info() -> String {
    let build_info: String = format!(
        "TON Node, version {}\n\
        Rust: {}\n\
        TON NODE git commit:         {}\n\
        ADNL git commit:             {}\n\
        DHT git commit:              {}\n\
        OVERLAY git commit:          {}\n\
        RLDP git commit:             {}\n\
        TON_BLOCK git commit:        {}\n\
        TON_BLOCK_JSON git commit:   {}\n\
        TON_SDK git commit:          {}\n\
        TON_EXECUTOR git commit:     {}\n\
        TON_TL git commit:           {}\n\
        TON_TYPES git commit:        {}\n\
        TON_VM git commit:           {}\n\
        TON_LABS_ABI git commit:     {}\n",
        std::option_env!("CARGO_PKG_VERSION").unwrap_or("Not set"),
        std::option_env!("RUST_VERSION").unwrap_or("Not set"),
        std::option_env!("GC_TON_NODE").unwrap_or("Not set"),
        std::option_env!("GC_ADNL").unwrap_or("Not set"),
        std::option_env!("GC_DHT").unwrap_or("Not set"),
        std::option_env!("GC_OVERLAY").unwrap_or("Not set"),
        std::option_env!("GC_RLDP").unwrap_or("Not set"),
        std::option_env!("GC_TON_BLOCK").unwrap_or("Not set"),
        std::option_env!("GC_TON_BLOCK_JSON").unwrap_or("Not set"),
        std::option_env!("GC_TON_SDK").unwrap_or("Not set"),
        std::option_env!("GC_TON_EXECUTOR").unwrap_or("Not set"),
        std::option_env!("GC_TON_TL").unwrap_or("Not set"),
        std::option_env!("GC_TON_TYPES").unwrap_or("Not set"),
        std::option_env!("GC_TON_VM").unwrap_or("Not set"),
        std::option_env!("GC_TON_LABS_ABI").unwrap_or("Not set")
    );
    return build_info;
}

#[cfg(feature = "external_db")]
fn start_external_db(config: &TonNodeConfig) -> Result<Vec<Arc<dyn ExternalDb>>> {
    Ok(vec!(
        external_db::create_external_db(
            config.external_db_config().ok_or_else(|| error!("Can't load external database config!"))?,
            config.front_workchain_ids()
        )?
    ))
}

#[cfg(not(feature = "external_db"))]
fn start_external_db(_config: &TonNodeConfig) -> Result<Vec<Arc<dyn ExternalDb>>> {
    Ok(vec!())
}

async fn start_engine(config: TonNodeConfig, zerostate_path: Option<&str>, initial_sync_disabled: bool) -> Result<()> {
    let external_db = start_external_db(&config)?;
    crate::engine::run(config, zerostate_path, external_db, initial_sync_disabled).await?;
    Ok(())
}

const CONFIG_NAME: &str = "config.json";
const DEFAULT_CONFIG_NAME: &str = "default_config.json";

fn main() {
    println!("{}", print_build_info());

    let app = clap::App::new("TON node")
        .arg(clap::Arg::with_name("zerostate")
            .short("z")
            .long("zerostate")
            .value_name("zerostate"))
        .arg(clap::Arg::with_name("config")
            .short("c")
            .long("configs")
            .value_name("config")
            .default_value("./"))
        .arg(clap::Arg::with_name("console_key")
            .short("k")
            .long("ckey")
            .value_name("console_key")
            .help("use console key in json format"))
        .arg(clap::Arg::with_name("initial_sync_disabled")
            .short("i")
            .long("initial-sync-disabled")
            .value_name("initial sync disable flag")
            .help("use this flag to sync from zero_state"));

    let matches = app.get_matches();

    let initial_sync_disabled = matches.is_present("initial_sync_disabled");

    let config_dir_path = match matches.value_of("config") {
        Some(config) => {
            config
        }
        None => {
            println!("Can't load config: config dir is not set!");
            return;
        }
    };

    let console_key = matches.value_of("console_key").map(|console_key| console_key.to_string());

    let zerostate_path = matches.value_of("zerostate");
    let config = match TonNodeConfig::from_file(
        config_dir_path, 
        CONFIG_NAME, 
        None,
        DEFAULT_CONFIG_NAME,
        console_key
    ) {
        Err(e) => {
            println!("Can't load config: {:?}", e);
            return;
        },
        Ok(c) => c
    };

    init_logger(config.log_config_path());
    log_version();
    
    lazy_static::initialize(&STATSD);
    
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .expect("Can't create tokio runtime");

    init_jaeger();
    
    runtime.block_on(async move {
        if let Err(e) = start_engine(config, zerostate_path, initial_sync_disabled).await {
            log::error!("Can't start node's Engine: {:?}", e);
        }
    });
}
