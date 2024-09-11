#![allow(clippy::type_complexity)]

use core::pin::pin;
use core::time::Duration;

use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use clap::Parser;
use futures::StreamExt as _;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{error, info, instrument, warn, Instrument as _, Span};
use url::Url;
use wasi_preview1_component_adapter_provider::{
    WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, WASI_SNAPSHOT_PREVIEW1_COMMAND_ADAPTER,
    WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
};
use wasmtime::component::{types, Component, InstancePre, Linker, ResourceType};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtxBuilder};
use wasmtime_wasi::{WasiCtx, WasiView};
use wrpc_runtime_wasmtime::{
    collect_component_resources, link_item, ServeExt as _, SharedResourceTable, WrpcView,
};
use wrpc_transport::Invoke;

const DEFAULT_TIMEOUT: &str = "10s";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
enum Command {
    Run(RunArgs),
    Serve(ServeArgs),
}

/// Run a command component
#[derive(Parser, Debug)]
struct RunArgs {
    /// NATS address to use
    #[arg(short, long, default_value = wrpc_cli::nats::DEFAULT_URL)]
    nats: String,

    /// Invocation timeout
    #[arg(long, default_value = DEFAULT_TIMEOUT)]
    timeout: humantime::Duration,

    /// Target prefix to send invocations to
    target: String,

    /// Path or URL to Wasm command component
    workload: String,
}

/// Serve a reactor component
#[derive(Parser, Debug)]
struct ServeArgs {
    /// NATS address to use
    #[arg(short, long, default_value = wrpc_cli::nats::DEFAULT_URL)]
    nats: String,

    /// Invocation timeout
    #[arg(long, default_value = DEFAULT_TIMEOUT)]
    timeout: humantime::Duration,

    /// NATS queue group to use
    #[arg(short, long)]
    group: Option<String>,

    /// Target prefix to send invocations to
    target: String,

    /// Prefix to listen on
    prefix: String,

    /// Path or URL to Wasm command component
    workload: String,
}

pub enum Workload {
    Url(Url),
    Binary(Vec<u8>),
}

pub struct Ctx<C: Invoke> {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub wrpc: C,
    pub shared_resources: SharedResourceTable,
    pub timeout: Duration,
}

impl<C: Invoke> WrpcView for Ctx<C> {
    type Invoke = C;

    fn client(&self) -> &Self::Invoke {
        &self.wrpc
    }

    fn shared_resources(&mut self) -> &mut SharedResourceTable {
        &mut self.shared_resources
    }

    fn timeout(&self) -> Option<Duration> {
        Some(self.timeout)
    }
}

impl<C: Invoke> WasiView for Ctx<C> {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[instrument(level = "trace", skip(adapter, cx))]
async fn instantiate_pre<C>(
    adapter: &[u8],
    cx: C::Context,
    workload: &str,
) -> anyhow::Result<(InstancePre<Ctx<C>>, Engine, Arc<[ResourceType]>)>
where
    C: Invoke,
    C::Context: Clone + 'static,
{
    let engine = Engine::new(
        wasmtime::Config::new()
            .async_support(true)
            .wasm_component_model(true),
    )
    .context("failed to initialize Wasmtime engine")?;

    let wasm = if workload.starts_with('.') {
        fs::read(&workload)
            .await
            .with_context(|| format!("failed to read relative path to workload `{workload}`"))
            .map(Workload::Binary)
    } else {
        Url::parse(workload)
            .with_context(|| format!("failed to parse Wasm URL `{workload}`"))
            .map(Workload::Url)
    }?;
    let wasm = match wasm {
        Workload::Url(wasm) => match wasm.scheme() {
            "file" => {
                let wasm = wasm
                    .to_file_path()
                    .map_err(|()| anyhow!("failed to convert Wasm URL to file path"))?;
                fs::read(wasm)
                    .await
                    .context("failed to read Wasm from file URL")?
            }
            "http" | "https" => {
                let wasm = reqwest::get(wasm).await.context("failed to GET Wasm URL")?;
                let wasm = wasm.bytes().await.context("failed fetch Wasm from URL")?;
                wasm.to_vec()
            }
            scheme => bail!("URL scheme `{scheme}` not supported"),
        },
        Workload::Binary(wasm) => wasm,
    };
    let wasm = if wasmparser::Parser::is_core_wasm(&wasm) {
        wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&wasm)
            .context("failed to set core component module")?
            .adapter(WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, adapter)
            .context("failed to add WASI adapter")?
            .encode()
            .context("failed to encode a component")?
    } else {
        wasm
    };

    let component = Component::new(&engine, wasm).context("failed to compile component")?;

    let mut linker = Linker::<Ctx<C>>::new(&engine);
    wasmtime_wasi::add_to_linker_async(&mut linker).context("failed to link WASI")?;

    let ty = component.component_type();
    let mut resources = Vec::new();
    collect_component_resources(&engine, &ty, &mut resources);
    let resources = Arc::from(resources);
    for (name, item) in ty.imports(&engine) {
        // Avoid polyfilling instances, for which static bindings are linked
        match name {
            "wasi:cli/environment@0.2.0"
            | "wasi:cli/exit@0.2.0"
            | "wasi:cli/stderr@0.2.0"
            | "wasi:cli/stdin@0.2.0"
            | "wasi:cli/stdout@0.2.0"
            | "wasi:cli/terminal-input@0.2.0"
            | "wasi:cli/terminal-output@0.2.0"
            | "wasi:cli/terminal-stderr@0.2.0"
            | "wasi:cli/terminal-stdin@0.2.0"
            | "wasi:cli/terminal-stdout@0.2.0"
            | "wasi:clocks/monotonic-clock@0.2.0"
            | "wasi:clocks/wall-clock@0.2.0"
            | "wasi:filesystem/preopens@0.2.0"
            | "wasi:filesystem/types@0.2.0"
            | "wasi:http/incoming-handler@0.2.0"
            | "wasi:http/outgoing-handler@0.2.0"
            | "wasi:http/types@0.2.0"
            | "wasi:io/error@0.2.0"
            | "wasi:io/poll@0.2.0"
            | "wasi:io/streams@0.2.0"
            | "wasi:random/random@0.2.0"
            | "wasi:sockets/instance-network@0.2.0"
            | "wasi:sockets/network@0.2.0"
            | "wasi:sockets/tcp-create-socket@0.2.0"
            | "wasi:sockets/tcp@0.2.0"
            | "wasi:sockets/udp-create-socket@0.2.0"
            | "wasi:sockets/udp@0.2.0" => continue,
            _ => {}
        }
        if let Err(err) = link_item(
            &engine,
            &mut linker.root(),
            Arc::clone(&resources),
            item,
            "",
            name,
            cx.clone(),
        ) {
            error!(?err, "failed to polyfill instance");
        }
    }

    let pre = linker
        .instantiate_pre(&component)
        .context("failed to pre-instantiate component")?;
    Ok((pre, engine, resources))
}

fn new_store<C: Invoke>(
    engine: &Engine,
    wrpc: C,
    arg0: &str,
    timeout: Duration,
) -> wasmtime::Store<Ctx<C>> {
    Store::new(
        engine,
        Ctx {
            wasi: WasiCtxBuilder::new()
                .inherit_env()
                .inherit_stdio()
                .inherit_network()
                .args(&[arg0])
                .build(),
            table: ResourceTable::new(),
            shared_resources: SharedResourceTable::default(),
            wrpc,
            timeout,
        },
    )
}

#[instrument(level = "trace", ret)]
pub async fn handle_run(
    RunArgs {
        nats,
        timeout,
        target,
        ref workload,
    }: RunArgs,
) -> anyhow::Result<()> {
    let nats = wrpc_cli::nats::connect(nats)
        .await
        .context("failed to connect to NATS")?;

    let (pre, engine, _) =
        instantiate_pre(WASI_SNAPSHOT_PREVIEW1_COMMAND_ADAPTER, None, workload).await?;
    let mut store = new_store(
        &engine,
        wrpc_transport_nats::Client::new(nats, target, None),
        "command.wasm",
        *timeout,
    );
    let cmd = wasmtime_wasi::bindings::CommandPre::new(pre)
        .context("failed to construct `command` instance")?
        .instantiate_async(&mut store)
        .await
        .context("failed to instantiate `command`")?;
    cmd.wasi_cli_run()
        .call_run(&mut store)
        .await
        .context("failed to run component")?
        .map_err(|()| anyhow!("component failed"))
}

#[instrument(level = "trace", skip_all, ret)]
pub async fn serve_shared(
    handlers: &mut JoinSet<()>,
    srv: wrpc_transport_nats::Client,
    mut store: wasmtime::Store<Ctx<wrpc_transport_nats::Client>>,
    pre: InstancePre<Ctx<wrpc_transport_nats::Client>>,
    guest_resources: Arc<[ResourceType]>,
) -> anyhow::Result<()> {
    let instance = pre
        .instantiate_async(&mut store)
        .await
        .context("failed to instantiate component")?;
    let engine = store.engine().clone();
    let store = Arc::new(Mutex::new(store));
    for (name, ty) in pre.component().component_type().exports(&engine) {
        match (name, ty) {
            (name, types::ComponentItem::ComponentFunc(ty)) => {
                info!(?name, "serving root function");
                let invocations = srv
                    .serve_function_shared(
                        Arc::clone(&store),
                        instance,
                        Arc::clone(&guest_resources),
                        ty,
                        "",
                        name,
                    )
                    .await?;
                handlers.spawn(async move {
                    let mut invocations = pin!(invocations);
                    while let Some(invocation) = invocations.next().await {
                        match invocation {
                            Ok((headers, fut)) => {
                                info!(?headers, "serving root function invocation");
                                if let Err(err) = fut.await {
                                    warn!(
                                        ?headers,
                                        ?err,
                                        "failed to serve root function invocation"
                                    );
                                } else {
                                    info!("successfully served root function invocation");
                                }
                            }
                            Err(err) => {
                                error!(?err, "failed to accept root function invocation");
                            }
                        }
                    }
                });
            }
            (_, types::ComponentItem::CoreFunc(_)) => {
                warn!(name, "serving root core function exports not supported yet");
            }
            (_, types::ComponentItem::Module(_)) => {
                warn!(name, "serving root module exports not supported yet");
            }
            (_, types::ComponentItem::Component(_)) => {
                warn!(name, "serving root component exports not supported yet");
            }
            (instance_name, types::ComponentItem::ComponentInstance(ty)) => {
                for (name, ty) in ty.exports(&engine) {
                    match ty {
                        types::ComponentItem::ComponentFunc(ty) => {
                            info!(?name, "serving instance function");
                            let invocations = srv
                                .serve_function_shared(
                                    Arc::clone(&store),
                                    instance,
                                    Arc::clone(&guest_resources),
                                    ty,
                                    instance_name,
                                    name,
                                )
                                .await?;
                            handlers.spawn(async move {
                                let mut invocations = pin!(invocations);
                                while let Some(invocation) = invocations.next().await {
                                    match invocation {
                                        Ok((headers, fut)) => {
                                            info!(?headers, "serving instance function invocation");
                                            if let Err(err) = fut.await {
                                                warn!(
                                                    ?headers,
                                                    ?err,
                                                    "failed to serve instance function invocation"
                                                );
                                            } else {
                                                info!(
                                                    "successfully served instance function invocation"
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            error!(
                                                ?err,
                                                "failed to accept instance function invocation"
                                            );
                                        }
                                    }
                                }
                            });
                        }
                        types::ComponentItem::CoreFunc(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance core function exports not supported yet"
                            );
                        }
                        types::ComponentItem::Module(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance module exports not supported yet"
                            );
                        }
                        types::ComponentItem::Component(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance component exports not supported yet"
                            );
                        }
                        types::ComponentItem::ComponentInstance(_) => {
                            warn!(
                                instance_name,
                                name, "serving nested instance exports not supported yet"
                            );
                        }
                        types::ComponentItem::Type(_) | types::ComponentItem::Resource(_) => {}
                    }
                }
            }
            (_, types::ComponentItem::Type(_) | types::ComponentItem::Resource(_)) => {}
        }
    }
    Ok(())
}

#[instrument(level = "trace", skip_all, ret)]
pub async fn serve_stateless(
    handlers: &mut JoinSet<()>,
    srv: wrpc_transport_nats::Client,
    clt: wrpc_transport_nats::Client,
    pre: InstancePre<Ctx<wrpc_transport_nats::Client>>,
    engine: &Engine,
    timeout: Duration,
) -> anyhow::Result<()> {
    let span = Span::current();
    for (name, ty) in pre.component().component_type().exports(engine) {
        match (name, ty) {
            (name, types::ComponentItem::ComponentFunc(ty)) => {
                let clt = clt.clone();
                let engine = engine.clone();
                info!(?name, "serving root function");
                let invocations = srv
                    .serve_function(
                        move || new_store(&engine, clt.clone(), "reactor.wasm", timeout),
                        pre.clone(),
                        ty,
                        "",
                        name,
                    )
                    .await?;
                handlers.spawn(
                    async move {
                        let mut invocations = pin!(invocations);
                        while let Some(invocation) = invocations.next().await {
                            match invocation {
                                Ok((headers, fut)) => {
                                    info!(?headers, "serving root function invocation");
                                    if let Err(err) = fut.await {
                                        warn!(
                                            ?headers,
                                            ?err,
                                            "failed to serve root function invocation"
                                        );
                                    } else {
                                        info!("successfully served root function invocation");
                                    }
                                }
                                Err(err) => {
                                    error!(?err, "failed to accept root function invocation");
                                }
                            }
                        }
                    }
                    .instrument(span.clone()),
                );
            }
            (_, types::ComponentItem::CoreFunc(_)) => {
                warn!(name, "serving root core function exports not supported yet");
            }
            (_, types::ComponentItem::Module(_)) => {
                warn!(name, "serving root module exports not supported yet");
            }
            (_, types::ComponentItem::Component(_)) => {
                warn!(name, "serving root component exports not supported yet");
            }
            (instance_name, types::ComponentItem::ComponentInstance(ty)) => {
                for (name, ty) in ty.exports(engine) {
                    match ty {
                        types::ComponentItem::ComponentFunc(ty) => {
                            let clt = clt.clone();
                            let engine = engine.clone();
                            info!(?name, "serving instance function");
                            let invocations = srv
                                .serve_function(
                                    move || {
                                        new_store(&engine, clt.clone(), "reactor.wasm", timeout)
                                    },
                                    pre.clone(),
                                    ty,
                                    instance_name,
                                    name,
                                )
                                .await?;
                            handlers.spawn(async move {
                                let mut invocations = pin!(invocations);
                                while let Some(invocation) = invocations.next().await {
                                    match invocation {
                                        Ok((headers, fut)) => {
                                            info!(?headers, "serving instance function invocation");
                                            if let Err(err) = fut.await {
                                                warn!(
                                                    ?headers,
                                                    ?err,
                                                    "failed to serve instance function invocation"
                                                );
                                            } else {
                                                info!(
                                                    "successfully served instance function invocation"
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            error!(
                                                ?err,
                                                "failed to accept instance function invocation"
                                            );
                                        }
                                    }
                                }
                            }.instrument(span.clone()));
                        }
                        types::ComponentItem::CoreFunc(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance core function exports not supported yet"
                            );
                        }
                        types::ComponentItem::Module(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance module exports not supported yet"
                            );
                        }
                        types::ComponentItem::Component(_) => {
                            warn!(
                                instance_name,
                                name, "serving instance component exports not supported yet"
                            );
                        }
                        types::ComponentItem::ComponentInstance(_) => {
                            warn!(
                                instance_name,
                                name, "serving nested instance exports not supported yet"
                            );
                        }
                        types::ComponentItem::Type(_) | types::ComponentItem::Resource(_) => {}
                    }
                }
            }
            (_, types::ComponentItem::Type(_) | types::ComponentItem::Resource(_)) => {}
        }
    }
    Ok(())
}

#[instrument(level = "trace", ret)]
pub async fn handle_serve(
    ServeArgs {
        nats,
        timeout,
        prefix,
        target,
        group,
        ref workload,
    }: ServeArgs,
) -> anyhow::Result<()> {
    let nats = wrpc_cli::nats::connect(nats)
        .await
        .context("failed to connect to NATS")?;
    let nats = Arc::new(nats);

    let (pre, engine, guest_resources) =
        instantiate_pre(WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER, None, workload).await?;

    let clt = wrpc_transport_nats::Client::new(Arc::clone(&nats), target, None);
    let srv = wrpc_transport_nats::Client::new(nats, prefix, group.map(Arc::from));
    let mut handlers = JoinSet::new();
    if guest_resources.is_empty() {
        serve_stateless(&mut handlers, srv, clt, pre, &engine, *timeout).await?;
    } else {
        serve_shared(
            &mut handlers,
            srv,
            new_store(&engine, clt, "reactor.wasm", *timeout),
            pre,
            guest_resources,
        )
        .await?;
    }
    while let Some(res) = handlers.join_next().await {
        if let Err(err) = res {
            error!(?err, "handler failed");
        }
    }
    Ok(())
}

#[instrument(level = "trace", ret)]
pub async fn run() -> anyhow::Result<()> {
    wrpc_cli::tracing::init();
    match Command::parse() {
        Command::Run(args) => handle_run(args).await,
        Command::Serve(args) => handle_serve(args).await,
    }
}
